import asyncio
import base64
import unittest

from sidevoice.transcribers import BrowserTranscriber, OpenAITranscriber, Transcript


class BrowserTranscriberTests(unittest.TestCase):
    def test_the_browser_gets_the_wav_and_its_answer_settles_the_request(self):
        async def run():
            sent = []
            provider = BrowserTranscriber(sent.append, 'session-1', language='es')
            pending = asyncio.create_task(provider.transcribe(b'RIFFwav'))
            await asyncio.sleep(0)
            request = sent[0]
            self.assertEqual(request['type'], 'voice-transcribe')
            self.assertEqual(request['data']['session_id'], 'session-1')
            self.assertEqual(request['data']['language'], 'es')
            self.assertEqual(base64.b64decode(request['data']['audio_base64']), b'RIFFwav')
            request_id = request['data']['request_id']
            self.assertFalse(provider.receive({'type': 'voice-transcript', 'data': {'request_id': 'someone-else', 'text': 'no'}}) is False)
            self.assertTrue(provider.receive({'type': 'voice-transcript', 'data': {
                'request_id': request_id, 'text': ' Hola ', 'metrics': {'recognition_ms': 90}}}))
            result = await asyncio.wait_for(pending, 1)
            self.assertEqual((result.text, result.metrics), ('Hola', {'recognition_ms': 90}))
            self.assertEqual(provider.pending, {})
        asyncio.run(run())

    def test_a_browser_error_fails_the_turn_and_unrelated_messages_are_not_ours(self):
        async def run():
            sent = []
            provider = BrowserTranscriber(sent.append, 'session-1')
            self.assertFalse(provider.receive({'type': 'voice-input-cancel', 'data': {}}))
            self.assertFalse(provider.receive('not a dict'))
            pending = asyncio.create_task(provider.transcribe(b'wav'))
            await asyncio.sleep(0)
            request_id = sent[0]['data']['request_id']
            provider.receive({'type': 'voice-transcript-error', 'data': {'request_id': request_id, 'error': 'GPU perdida'}})
            with self.assertRaisesRegex(RuntimeError, 'GPU perdida'):
                await asyncio.wait_for(pending, 1)
        asyncio.run(run())

    def test_a_browser_that_never_answers_times_out_instead_of_holding_the_turn(self):
        async def run():
            provider = BrowserTranscriber(lambda message: None, 'session-1', timeout=0.01)
            with self.assertRaises(asyncio.TimeoutError):
                await provider.transcribe(b'wav')
            self.assertEqual(provider.pending, {})
        asyncio.run(run())


class OpenAITranscriberTests(unittest.TestCase):
    def test_request_shape_and_confidence(self):
        class Client:
            def __init__(self):
                self.calls = []
                outer = self
                class Transcriptions:
                    async def create(self, **kwargs):
                        outer.calls.append(kwargs)
                        return type('Response', (), {'text': ' Hola ', 'logprobs': [{'logprob': -0.5}, {'logprob': -1.5}]})()
                self.audio = type('Audio', (), {'transcriptions': Transcriptions()})()
        async def run():
            client = Client()
            provider = OpenAITranscriber('sk-unused', model='gpt-4o-transcribe', language='es', prompt='Sidevoice', client=client)
            result = await provider.transcribe(b'wav')
            self.assertEqual(result, Transcript('Hola', -1.0))
            call = client.calls[0]
            self.assertEqual((call['model'], call['language'], call['prompt'], call['response_format'], call['include']),
                             ('gpt-4o-transcribe', 'es', 'Sidevoice', 'json', ['logprobs']))
            whisper = OpenAITranscriber('sk-unused', model='whisper-1', client=client)
            await whisper.transcribe(b'wav')
            self.assertEqual(client.calls[1]['response_format'], 'verbose_json')
            self.assertNotIn('language', client.calls[1])
        asyncio.run(run())


if __name__ == '__main__':
    unittest.main()
