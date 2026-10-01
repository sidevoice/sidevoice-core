"""A provider's model checked before a client makes it its stage (sidevoice/sidevoice-core#21, sidevoice/sidevoice-core#13): the same check a device runs on
the models it runs itself, run here because the key is here.

The node calls the provider with its own key, twice — the first pass warms the connection up, the second is
the one measured — on the clip or phrase of `sidevoice_core.models.verdicts`, and judges the answer by the
same thresholds. A transcription is measured by its turn-final latency (the whole clip sent, the text back);
a voice by its time to first audio and how fast it generates against real time. It costs the account a
fraction of a cent.

The answer is never an exception: `{ok, step, reason, passes, …}`, where a failure names the step that failed
(`key`: there is none, or the provider refused it; `check`: the provider failed, or what it gave back does not
pass) and the reason as a refusal a client translates (`{key, message, …params}`).
"""
import array
import asyncio
import base64
import sys
import time

from . import integrations
from ..models import verdicts

PASSES = 2


def _refusal(key, message, **params):
    return {'key': key, 'message': message, **params}


def _failed(step, reason, passes):
    return {'ok': False, 'step': step, 'reason': reason, 'passes': passes}


def _label(provider):
    from .settings import PROVIDERS
    return PROVIDERS.get(provider, {}).get('label', provider)


async def check(stage, *, language=None, config=None):
    """Check `stage` — a validated provider stage (`settings.Transcription` or `settings.Voice`) — with the
    node's key for its provider. `language` is the speech language to check in; a transcription stage's own
    language option wins over it."""
    provider = stage.place
    key = integrations.key(provider, config)
    if not key:
        return _failed('key', _refusal('provider_key_missing', f'{_label(provider)} needs an API key before connecting.',
                                       provider=provider), [])
    if stage.TASK == 'stt':
        own = stage.options.get('language')
        return await _transcription(stage, key, own if own not in (None, 'auto') else language)
    return await _voice(stage, key, language, config)


def _provider_failure(provider, error):
    """An exception from a provider's client as the step it failed and the refusal it means."""
    status = getattr(error, 'status', None) or getattr(error, 'status_code', None)
    name = type(error).__name__
    if status in (401, 403) or name in ('AuthenticationError', 'PermissionDeniedError'):
        return 'key', _refusal('provider_key_refused', f'{_label(provider)} refused the key.', provider=provider)
    if status is None and (name in ('APIConnectionError', 'APITimeoutError', 'ProviderUnreachable', 'TimeoutError')
                           or isinstance(error, (asyncio.TimeoutError, OSError))):
        return 'check', _refusal('provider_unreachable', f'{_label(provider)} could not be reached.', provider=provider)
    detail = str(status) if status is not None else name
    return 'check', _refusal('provider_failed', f'{_label(provider)} failed: {detail}.', provider=provider, detail=detail)


async def _transcription(stage, key, language):
    from .transcribers import OpenAITranscriber
    language = verdicts.language_for('stt', language)
    audio, expected = verdicts.clip(language)
    transcriber = OpenAITranscriber(key, model=stage.model, language=language)
    passes = []
    for _ in range(PASSES):
        started = time.monotonic()
        try:
            result = await transcriber.transcribe(audio)
        except Exception as error:   # noqa: BLE001 - every provider failure is an answer, never a 500
            step, reason = _provider_failure(stage.place, error)
            return _failed(step, reason, passes)
        latency = round((time.monotonic() - started) * 1000)
        passes.append({'latency_ms': latency, 'text': result.text})
        if problem := verdicts.transcript_problem(expected, result.text):
            return _failed('check', problem, passes)
    measured = passes[-1]['latency_ms']
    return {'ok': True, 'step': 'done', 'language': language, 'passes': passes,
            'latency_ms': measured, 'slow': verdicts.slow(measured)}


def _pcm(audio_base64):
    """ElevenLabs' `pcm_16000`: little-endian 16-bit mono at 16 kHz, as floats."""
    raw = base64.b64decode(audio_base64)
    values = array.array('h', raw[:len(raw) - len(raw) % 2])
    if sys.byteorder == 'big':
        values.byteswap()
    return [value / 32768 for value in values]


async def _voice(stage, key, language, config):
    from . import synthesis
    language = verdicts.language_for('tts', language)
    chosen = stage.options.get('voice') or {}
    voice = chosen.get(language) if isinstance(chosen, dict) else chosen
    voice = voice or (next(iter(chosen.values()), None) if isinstance(chosen, dict) else None)
    if not voice:
        return _failed('check', _refusal('voice_missing', f'Choose a voice for {_label(stage.place)} before connecting.',
                                         provider=stage.place), [])
    text = verdicts.phrase(language)
    passes = []
    for _ in range(PASSES):
        try:
            answer = await synthesis.synthesize(text, model=stage.model, voice=voice, speed=stage.options.get('speed', 1),
                                                config=config, output_format='pcm_16000')
        except Exception as error:   # noqa: BLE001 - every provider failure is an answer, never a 500
            if type(error) is ValueError and 'no audio' in str(error):
                return _failed('check', _refusal('check_silent', 'The model loaded but produced nothing.'), passes)
            step, reason = _provider_failure(stage.place, error)
            return _failed(step, reason, passes)
        samples = _pcm(answer['audio_base64'])
        timings = answer.get('timings_ms') or {}
        total = timings.get('request_to_complete_ms') or 0
        seconds = len(samples) / 16000
        passes.append({'first_audio_ms': round(timings.get('request_to_first_chunk_ms') or total),
                       'total_ms': round(total), 'audio_seconds': round(seconds, 2),
                       'realtime': round(seconds * 1000 / total, 2) if total else None})
        if problem := verdicts.audio_problem(samples, 16000):
            return _failed('check', problem, passes)
    return {'ok': True, 'step': 'done', 'language': language, 'voice': voice, 'passes': passes,
            'latency_ms': passes[-1]['first_audio_ms'], 'slow': False}
