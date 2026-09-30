import asyncio
from io import BytesIO
from types import SimpleNamespace
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import AsyncMock, patch
import wave

import numpy as np

from sidevoice.speech_filter import SegmentSpeechGate, unreliable_transcription
from sidevoice.transcribers import Transcript, TurnTranscriber


class FakeProvider:
    def __init__(self, result):
        self.result, self.received = result, []

    async def transcribe(self, wav):
        self.received.append(wav)
        return self.result


def turn_transcriber(provider, **kwargs):
    service = TurnTranscriber(provider, **kwargs)
    service._sample_rate = 16000
    service._audio_buffer_size_1s = 32000
    service._record_stt_audio_usage = lambda pcm: None
    service.emit_stt_usage_metrics = AsyncMock()
    return service


def wav(samples, rate=16000):
    output = BytesIO()
    with wave.open(output, 'wb') as stream:
        stream.setnchannels(1); stream.setsampwidth(2); stream.setframerate(rate)
        stream.writeframes(np.asarray(samples, dtype='<i2').tobytes())
    return output.getvalue()


class SpeechFilterTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.gate = SegmentSpeechGate()

    def test_real_vad_rejects_silence_noise_and_clicks(self):
        rng = np.random.default_rng(42)
        click = np.zeros(32000); click[1000:1010] = 25000
        for samples in [np.zeros(32000), rng.normal(0, 300, 32000), click]:
            with self.subTest(kind=float(np.max(samples))):
                self.assertFalse(self.gate.assess(wav(samples)).accepted)

    def test_short_commands_are_not_rejected_by_text_confidence(self):
        for text in ['Para.', 'No.', 'No lo hagas']:
            score = -0.1
            self.assertFalse(unreliable_transcription(SimpleNamespace(
                text=text, logprobs=[{'logprob': score}])))

    @unittest.skipUnless(sys.platform == 'darwin', 'Uses the local macOS voice')
    def test_real_short_speech_and_repetitions_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'command.wav'
            for command in ['No', 'Para', 'Sí']:
                subprocess.run(['/usr/bin/say', '-v', 'Mónica', '-o', str(target),
                                '--file-format=WAVE', '--data-format=LEI16@16000', command], check=True)
                audio = target.read_bytes()
                # The same instruction can legitimately be repeated.
                self.assertTrue(self.gate.assess(audio).accepted, command)
                self.assertTrue(self.gate.assess(audio).accepted, command)

    def test_isolated_scripts_rejected_but_mentions_preserved(self):
        from sidevoice.speech_filter import isolated_foreign_script
        for text in ['咳咳', 'うん', 'لنهاية.', '「東京」']:
            self.assertTrue(isolated_foreign_script(text), text)
        for text in ['¿Qué significa 咳咳?', 'Translate 東京 please', 'No', 'Sí',
                     'Pokémon', 'GPT-5', 'Prueba terminada', 'Se podería probar', '123']:
            self.assertFalse(isolated_foreign_script(text), text)

    def test_measured_background_noise_and_user_speech(self):
        # Measurements from the user's labeled background-noise test, 2026-09-13.
        self.assertFalse(self.gate.decide(224, .783).accepted)
        self.assertFalse(self.gate.decide(192, .8487).accepted)
        self.assertTrue(self.gate.decide(800, .96).accepted)

    def test_low_confidence_fragments_are_rejected_in_any_script(self):
        for text in ['うん', 'لنهاية.', 'Eh', 'No']:
            self.assertTrue(unreliable_transcription(SimpleNamespace(
                text=text, logprobs=[{'logprob': -4.0}])), text)
        # Script alone is not evidence of noise or a reason to censor speech.
        self.assertFalse(unreliable_transcription(SimpleNamespace(
            text='うん', logprobs=[{'logprob': -0.1}])))

    def test_missing_confidence_is_not_evidence_of_bad_transcription(self):
        self.assertFalse(unreliable_transcription(SimpleNamespace(text='Una frase perfectamente válida')))
        self.assertTrue(unreliable_transcription(SimpleNamespace(
            text='Una frase poco fiable', logprobs=[{'logprob': -3}])) )

    def test_diagnostics_record_evidence_without_audio_or_text(self):
        from sidevoice.speech_filter import SpeechEvidence
        async def run():
            gate = SimpleNamespace(assess=lambda audio: SpeechEvidence(True, 'speech', 128, .8))
            service = turn_transcriber(FakeProvider(Transcript('Sorry', confidence=-4)), speech_gate=gate)
            service._turn_audio.extend(b'\0\0' * 1600)
            result = await service.transcribe_turn()
            self.assertEqual(result.text, '')
            self.assertEqual(service.filter_stats['recent_segments'], [{
                'speech_ms': 128, 'peak_probability': .8,
                'mean_logprob': -4, 'decision': 'low_confidence'}])
        asyncio.run(run())

    def test_rejected_audio_never_reaches_the_provider(self):
        async def run():
            provider = FakeProvider(Transcript('never'))
            service = turn_transcriber(provider, speech_gate=self.gate)
            service._turn_audio.extend(np.zeros(16000, dtype='<i2').tobytes())
            result = await service.transcribe_turn()
            self.assertEqual(result.text, '')
            self.assertEqual(provider.received, [])
            self.assertEqual(service.filter_stats['audio_rejected'], 1)
        asyncio.run(run())

    def test_short_vad_pauses_are_batched_into_one_complete_turn(self):
        async def run():
            provider = FakeProvider(Transcript('Hola, sigo hablando'))
            service = turn_transcriber(provider, speech_gate=SimpleNamespace(
                assess=lambda audio: __import__('sidevoice.speech_filter', fromlist=['SpeechEvidence']).SpeechEvidence(True, 'speech', 500, .95)))
            first = np.full(1600, 1000, dtype='<i2').tobytes()
            second = np.full(1600, 2000, dtype='<i2').tobytes()

            await service._handle_user_started_speaking(None)
            service._audio_buffer.extend(first)
            await service._handle_user_stopped_speaking(None)
            self.assertIsNone(service.provider.received or None)
            await service._handle_user_started_speaking(None)
            service._audio_buffer.extend(second)
            await service._handle_user_stopped_speaking(None)

            result = await service.transcribe_turn()
            self.assertEqual(result.text, 'Hola, sigo hablando')
            self.assertEqual(len(provider.received), 1)
            with wave.open(BytesIO(provider.received[0]), 'rb') as stream:
                content = stream.readframes(stream.getnframes())
            self.assertEqual(content[:len(first)], first)
            self.assertEqual(content[len(first):len(first) + len(second)], second)
            self.assertEqual(len(content), len(first) + len(second) + 16000)
            service.emit_stt_usage_metrics.assert_awaited_once()
            self.assertIsNone(await service.transcribe_turn())
        asyncio.run(run())

    def test_a_turn_that_went_idle_mid_speech_still_uses_the_live_buffer(self):
        async def run():
            provider = FakeProvider(Transcript('Corte'))
            service = turn_transcriber(provider, speech_gate=SimpleNamespace(
                assess=lambda audio: __import__('sidevoice.speech_filter', fromlist=['SpeechEvidence']).SpeechEvidence(True, 'speech', 500, .95)))
            await service._handle_user_started_speaking(None)
            service._audio_buffer.extend(np.full(1600, 500, dtype='<i2').tobytes())
            result = await service.transcribe_turn()
            self.assertEqual(result.text, 'Corte')
            self.assertEqual(len(service._audio_buffer), 0)
        asyncio.run(run())


if __name__ == '__main__':
    unittest.main()
