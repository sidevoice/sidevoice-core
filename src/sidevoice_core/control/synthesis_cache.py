"""Audio the room pays for once and every listener reuses.

Kokoro runs inside each browser: it costs nothing per listener and there is
nothing to share. ElevenLabs is billed per character, so rendering the same reply
once per person looking at the room is a defect, not a detail. This cache renders
one utterance once — keyed by everything that could change the audio — keeps the
encoded result in a bounded LRU and hands every client the same bytes and the
same character alignment, so karaoke stays identical without a second request.

What it never shares is the *measurement*: a listener handed someone else's
render did not wait for the provider, and `obtain` says so.
"""
import asyncio
import hashlib
import json
from collections import OrderedDict


class SynthesisCache:
    """One render per configuration, shared by every listener, bounded in count and bytes."""

    def __init__(self, *, limit_items=64, limit_bytes=32 * 1024 * 1024, renderer=None):
        self.limit_items, self.limit_bytes = limit_items, limit_bytes
        self.entries = OrderedDict()
        self.inflight = {}
        self.bytes = 0
        self.renders = 0   # how many times a provider was actually paid
        self.reuses = 0    # how many listeners got one of those renders for free
        self.renderer = renderer or _render

    @staticmethod
    def key(choice, text):
        material = json.dumps([choice.get('provider'), choice.get('model'), choice.get('voice'),
                               choice.get('speed'), text], sort_keys=True, ensure_ascii=False)
        return hashlib.sha256(material.encode('utf8')).hexdigest()[:32]

    def read(self, key):
        entry = self.entries.get(key)
        if entry is not None:
            self.entries.move_to_end(key)
        return entry

    async def obtain(self, choice, text):
        """Return `(audio, fresh)`: `fresh` is False when this listener paid nothing."""
        key = self.key(choice, text)
        existing = self.read(key)
        if existing is not None:
            self.reuses += 1
            return existing, False
        task = self.inflight.get(key)
        if task is None:
            task = asyncio.ensure_future(self._render(key, choice, text))
            self.inflight[key] = task
            task.add_done_callback(lambda finished, k=key: self.inflight.pop(k, None))
            fresh = True
        else:
            self.reuses += 1
            fresh = False
        # Shielded: a listener that walks away mid-render must not cancel it for the others.
        return await asyncio.shield(task), fresh

    async def _render(self, key, choice, text):
        audio = await self.renderer(choice, text)
        self.renders += 1
        self.entries[key] = audio
        self.bytes += len(audio.get('audio_base64') or '')
        self._evict()
        return audio

    def _evict(self):
        while self.entries and (len(self.entries) > self.limit_items or self.bytes > self.limit_bytes):
            _, dropped = self.entries.popitem(last=False)
            self.bytes -= len(dropped.get('audio_base64') or '')

    def stats(self):
        return {'items': len(self.entries), 'bytes': self.bytes,
                'renders': self.renders, 'reuses': self.reuses}


async def _render(choice, text):
    from . import synthesis
    return await synthesis.synthesize(text, model=choice['model'], voice=choice['voice'],
                                      speed=choice['speed'], with_timestamps=True)
