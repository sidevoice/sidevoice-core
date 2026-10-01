"""The microphone over WebRTC: a client's audio track straight to this node, signalled through whatever
carries its requests (the room's relay, or nothing when the page talks to the node directly).

The call socket stays for everything it already carries — the session, turns, receipts, replies —
and stays the fallback: a page whose peer connection never connects, or drops, says `voice-media:
socket` and goes on sending PCM on it. What arrives
on the track is resampled to the pipeline's 16 kHz mono and fed into the same input the socket's
frames reach, so the detector, the turn and the transcription never learn which path it took.

Signalling is one request: the page gathers its ICE candidates, posts the offer, and gets an answer
with this node's candidates in it (no trickle: aiortc gathers before it answers). STUN only — no TURN
server exists — so two peers that cannot see each other stay on the socket.
"""
import asyncio
import os

from fastapi import HTTPException, Request
from loguru import logger
from pydantic import BaseModel, Field

DEFAULT_STUN = 'stun:stun.l.google.com:19302'
SAMPLE_RATE = 16000


def available():
    try:
        import aiortc  # noqa: F401
        import av  # noqa: F401
    except ImportError:
        return False
    return (os.environ.get('SIDEVOICE_WEBRTC') or 'on').lower() not in {'off', '0', 'false'}


def ice_urls():
    """`SIDEVOICE_STUN_URLS` (comma-separated; empty means host candidates only), else Google's public STUN."""
    named = os.environ.get('SIDEVOICE_STUN_URLS')
    urls = [DEFAULT_STUN] if named is None else [url.strip() for url in named.split(',') if url.strip()]
    return urls


class Offer(BaseModel):
    session_id: str = Field(min_length=1, max_length=64)
    sdp: str = Field(min_length=1, max_length=64_000)
    type: str = Field(pattern='^offer$')


class MediaPeer:
    """One client's peer connection: the track it sends, pumped into its call while the client says so."""

    def __init__(self, client):
        from aiortc import RTCConfiguration, RTCIceServer, RTCPeerConnection
        self.client = client
        urls = ice_urls()
        self.pc = RTCPeerConnection(RTCConfiguration(iceServers=[RTCIceServer(urls=urls)] if urls else []))
        self.pumps = set()
        self.frames = 0
        self.pc.on('track', self.track)
        self.pc.on('connectionstatechange', self.state_changed)

    def track(self, track):
        if track.kind != 'audio':
            return
        pump = asyncio.create_task(self.pump(track))
        self.pumps.add(pump)
        pump.add_done_callback(self.pumps.discard)

    async def state_changed(self):
        logger.info('Call {}: WebRTC {}', self.client.id[:8], self.pc.connectionState)
        if self.pc.connectionState in {'failed', 'closed'}:
            await self.close()

    async def pump(self, track):
        """Every frame of the track, as 16 kHz mono PCM, into the call — only while the client's microphone
        is on this path. A frame that arrives while the client says `socket` proves nothing and is dropped."""
        import av
        from aiortc.mediastreams import MediaStreamError
        resampler = av.AudioResampler(format='s16', layout='mono', rate=SAMPLE_RATE)
        serializer = self.client.mic
        while True:
            try:
                frame = await track.recv()
            except MediaStreamError:
                return
            if serializer is None or serializer.media_path != 'webrtc' or self.client.feed_audio is None:
                continue
            for piece in resampler.resample(frame):
                pcm = bytes(piece.planes[0])[:piece.samples * 2]
                if not pcm:
                    continue
                self.frames += 1
                serializer.heard(len(pcm))
                await self.client.feed_audio(pcm, SAMPLE_RATE)

    async def answer(self, sdp):
        from aiortc import RTCSessionDescription
        await self.pc.setRemoteDescription(RTCSessionDescription(sdp=sdp, type='offer'))
        await self.pc.setLocalDescription(await self.pc.createAnswer())
        return {'sdp': self.pc.localDescription.sdp, 'type': self.pc.localDescription.type}

    async def close(self):
        if self.client.media_peer is self:
            self.client.media_peer = None
        for pump in list(self.pumps):
            pump.cancel()
        await self.pc.close()


def mount_webrtc(app, room):
    @app.get('/api/presentation/rtc/config')
    async def rtc_config():
        enabled = available()
        return {'enabled': enabled, 'ice_servers': [{'urls': ice_urls()}] if enabled and ice_urls() else []}

    @app.post('/api/presentation/rtc/offer')
    async def rtc_offer(offer: Offer, request: Request):
        from .presentation import require_same_origin
        require_same_origin(request)
        if not available():
            raise HTTPException(503, 'This node does not take WebRTC; the microphone stays on the call socket.')
        client = room.clients.get(offer.session_id)
        if client is None or not client.connected or client.feed_audio is None:
            raise HTTPException(409, 'That call is not in this node\'s room.')
        if client.media_peer is not None:
            # A page negotiating again (a network change) replaces its previous connection, never adds one.
            await client.media_peer.close()
        peer = MediaPeer(client)
        client.media_peer = peer
        try:
            return await asyncio.wait_for(peer.answer(offer.sdp), 20)
        except Exception as error:
            await peer.close()
            raise HTTPException(422, 'Could not answer that offer: ' + (str(error) or type(error).__name__)) from error
