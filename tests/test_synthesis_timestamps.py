import base64
import json
import unittest
from unittest.mock import patch

from sidevoice import synthesis


class SynthesisTimestampsTest(unittest.IsolatedAsyncioTestCase):
    async def synthesize_response(self, response_body, status=200):
        requests = []

        class Response:
            async def __aenter__(self): return self
            async def __aexit__(self, *args): pass
            @property
            def content(self): return self
            async def iter_any(self):
                data = json.dumps(response_body).encode()
                yield data[:7]
                yield data[7:]

        class Client:
            async def __aenter__(self): return self
            async def __aexit__(self, *args): pass
            def post(self, url, **kwargs):
                requests.append((url, kwargs['json']))
                response = Response()
                response.status = status
                return response

        with patch.object(synthesis, 'key', return_value='test-only'), patch.object(
                synthesis.aiohttp, 'ClientSession', return_value=Client()):
            result = await synthesis.synthesize('Hola', model='test-model', voice='voice/id',
                                                speed=1.2, with_timestamps=True)
        return result, requests

    async def test_timestamps_endpoint_preserves_audio_alignment_and_native_speed(self):
        alignment = {'characters': list('Hola'), 'character_start_times_seconds': [0, .1, .2, .3],
                     'character_end_times_seconds': [.1, .2, .3, .4]}
        result, requests = await self.synthesize_response({
            'audio_base64': base64.b64encode(b'fake-mp3').decode(), 'alignment': alignment,
            'normalized_alignment': {'characters': ['not the original text']}})
        self.assertEqual(len(requests), 1)
        url, body = requests[0]
        self.assertIn('/voice%2Fid/with-timestamps?', url)
        self.assertEqual(body['voice_settings']['speed'], 1.2)
        self.assertEqual(base64.b64decode(result['audio_base64']), b'fake-mp3')
        self.assertEqual(result['alignment'], alignment)
        self.assertIn('request_to_complete_ms', result['timings_ms'])

    async def test_missing_alignment_does_not_break_audio_or_trigger_a_second_request(self):
        result, requests = await self.synthesize_response({'audio_base64': 'YQ=='})
        self.assertIsNone(result['alignment'])
        self.assertEqual(len(requests), 1)

    async def test_invalid_audio_is_rejected(self):
        for body in ({}, {'audio_base64': '?'}, {'audio_base64': ''}):
            with self.subTest(body=body), self.assertRaises(ValueError):
                await self.synthesize_response(body)
