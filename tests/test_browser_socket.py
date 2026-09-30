import json
from unittest import IsolatedAsyncioTestCase
from unittest.mock import patch
from pipecat.frames.frames import (InputAudioRawFrame, InputTransportMessageFrame, OutputAudioRawFrame,
                                   OutputTransportMessageFrame, OutputTransportMessageUrgentFrame, TextFrame)
from sidevoice.browser_socket import BrowserFrameSerializer, session_message


class BrowserSocketTest(IsolatedAsyncioTestCase):
    async def test_binary_frames_are_microphone_pcm_in_the_declared_format(self):
        frame = await BrowserFrameSerializer().deserialize(b'\x00\x01' * 320)
        self.assertIsInstance(frame, InputAudioRawFrame)
        self.assertEqual((frame.sample_rate, frame.num_channels, frame.num_frames), (16000, 1, 320))
        self.assertEqual(frame.audio, b'\x00\x01' * 320)

    async def test_a_dangling_byte_never_reaches_the_pipeline(self):
        frame = await BrowserFrameSerializer().deserialize(bytearray(b'\x00\x01\x02'))
        self.assertEqual(frame.audio, b'\x00\x01')
        self.assertIsNone(await BrowserFrameSerializer().deserialize(b'\x02'))

    async def test_packet_gaps_are_measured_without_changing_audio(self):
        serializer = BrowserFrameSerializer()
        with patch('sidevoice.browser_socket.time.monotonic', side_effect=[10.0, 10.020, 10.390, 10.410]):
            for _ in range(4):
                frame = await serializer.deserialize(b'\x00\x01' * 320)
                self.assertEqual(frame.audio, b'\x00\x01' * 320)
        self.assertEqual(serializer.audio_frames, 4)
        self.assertEqual(serializer.last_audio_gap_ms, 20)
        self.assertEqual(serializer.max_audio_gap_ms, 370)
        self.assertEqual(serializer.audio_gap_count, 1)

    async def test_anything_the_browser_sends_says_that_it_is_still_there(self):
        # The keepalive asks a quiet socket, not a muted one: PCM, an answer and even a frame the
        # room cannot read are all this browser being alive (#63).
        with patch('sidevoice.browser_socket.time.monotonic', side_effect=[10.0, 20.0, 30.0, 40.0]):
            serializer = BrowserFrameSerializer()
            self.assertEqual(serializer.last_frame_at, 10.0, 'the hello that opened the call counts')
            await serializer.deserialize(b'\x00\x01' * 320)
            self.assertEqual(serializer.last_frame_at, 20.0)
            await serializer.deserialize(json.dumps({'type': 'voice-pong', 'data': {'session_id': 'call'}}))
            self.assertEqual(serializer.last_frame_at, 30.0)
            await serializer.deserialize('{not json')
            self.assertEqual(serializer.last_frame_at, 40.0)

    async def test_text_frames_are_app_messages(self):
        ready = {'label': 'rtvi-ai', 'type': 'client-ready', 'id': '1', 'data': {}}
        frame = await BrowserFrameSerializer().deserialize(json.dumps(ready))
        self.assertIsInstance(frame, InputTransportMessageFrame)
        self.assertEqual(frame.message, ready)
        self.assertIsNone(await BrowserFrameSerializer().deserialize('{not json'))
        self.assertIsNone(await BrowserFrameSerializer().deserialize('[1, 2]'))

    async def test_room_events_reach_the_browser_in_the_shape_the_page_reads(self):
        event = {'type': 'voice-speech', 'data': {'session_id': 's', 'revision': 1, 'utterance_id': 'u', 'text': 'Hola'}}
        for frame in (OutputTransportMessageFrame(message=event), OutputTransportMessageUrgentFrame(message=event)):
            payload = await BrowserFrameSerializer().serialize(frame)
            self.assertIsInstance(payload, str)
            self.assertEqual(json.loads(payload), event)

    async def test_rtvi_events_are_delivered_not_filtered(self):
        rtvi = {'label': 'rtvi-ai', 'type': 'user-transcription', 'data': {'text': 'hola', 'final': True}}
        payload = await BrowserFrameSerializer().serialize(OutputTransportMessageUrgentFrame(message=rtvi))
        self.assertEqual(json.loads(payload), rtvi)

    async def test_nothing_else_crosses_to_the_browser(self):
        serializer = BrowserFrameSerializer()
        self.assertIsNone(await serializer.serialize(OutputAudioRawFrame(audio=b'\x00\x00', sample_rate=16000, num_channels=1)))
        self.assertIsNone(await serializer.serialize(TextFrame(text='x')))

    def test_the_call_announces_its_id_and_the_format_it_expects(self):
        message = session_message('call-1', BrowserFrameSerializer())
        room = message['data'].pop('room')
        self.assertEqual(message, {'type': 'voice-session', 'data': {'session_id': 'call-1', 'sample_rate': 16000, 'channels': 1}})
        self.assertEqual(set(room), {'version', 'web_build'}, 'the room also says what it is and which web build it serves')
