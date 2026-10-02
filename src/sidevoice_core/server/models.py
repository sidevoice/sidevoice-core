"""The model catalogue, as this node serves it to its clients (`sidevoice_core.models`), and the check a
provider's model passes before a client makes it its stage, plus a bounded one-shot transcription preview.

Behind a paired device's token like every node route, and relayed like the rest of `/api/models`. The catalogue
is served exactly as shipped, so what a client reads here is byte for byte what the web build and the desktop
app were generated from.
"""
import asyncio
import base64
import binascii
import math
import time
from collections import deque
from typing import Literal

from fastapi import HTTPException, Request, Response
from pydantic import BaseModel, ConfigDict, Field, ValidationError

from ..models.catalog import catalog_text
from .devices import DEVICE_KEY
from .presentation import require_same_origin

# A device checks the models it runs itself (it has the clips too); only a provider is checked from here.
CHECK_ON_DEVICE = {'key': 'check_on_device', 'message': 'A device checks the models it runs itself.'}

PREVIEW_BODY_LIMIT = 512 * 1024
PREVIEW_PCM_LIMIT = 10 * 16000 * 2
PREVIEW_PCM_MINIMUM = 250 * 16000 * 2 // 1000
PREVIEW_TIMEOUT = 30.0


class PreviewAudio(BaseModel):
    model_config = ConfigDict(extra='forbid')
    encoding: Literal['pcm_s16le']
    sample_rate: Literal[16000]
    data_base64: str


class PreviewOptions(BaseModel):
    model_config = ConfigDict(extra='forbid')
    language: str = Field(min_length=1, max_length=20)
    context: str | None = Field(default=None, max_length=400)


class PreviewPayload(BaseModel):
    model_config = ConfigDict(extra='forbid')
    place: str = Field(min_length=1, max_length=60)
    model: str = Field(min_length=1, max_length=120)
    options: PreviewOptions
    audio: PreviewAudio


class PreviewBusy(Exception):
    def __init__(self, retry_after):
        super().__init__('preview budget exhausted')
        self.retry_after = retry_after


class TranscriptionPreviewBudget:
    """One provider preview per device and at most six starts per device in a rolling minute."""

    def __init__(self, *, clock=time.monotonic, per_device=6, window_seconds=60.0):
        self.clock, self.per_device, self.window_seconds = clock, per_device, window_seconds
        self.starts = {}
        self.active = set()

    def start(self, device):
        now = self.clock()
        window = self.starts.setdefault(device, deque())
        while window and now - window[0] >= self.window_seconds:
            window.popleft()
        if not window:
            self.starts.pop(device, None)
            window = self.starts.setdefault(device, deque())
        if device in self.active:
            raise PreviewBusy(1)
        if len(window) >= self.per_device:
            raise PreviewBusy(max(1, math.ceil(self.window_seconds - (now - window[0]))))
        window.append(now)
        self.active.add(device)

    def finish(self, device):
        self.active.discard(device)


def _preview_refusal(status, key, message, *, retry_after=None):
    headers = {'Retry-After': str(retry_after)} if retry_after is not None else None
    raise HTTPException(status, {'key': key, 'message': message}, headers=headers)


async def _bounded_body(request):
    """Read one JSON body without ever accumulating more than the accepted encoded limit."""
    content_length = request.headers.get('content-length')
    if content_length is not None:
        try:
            if int(content_length) > PREVIEW_BODY_LIMIT:
                _preview_refusal(413, 'trial.audio_too_large', 'The recording exceeds the 10 second limit.')
        except ValueError:
            _preview_refusal(400, 'trial.invalid_stage', 'The preview request is malformed.')
    body = bytearray()
    async for chunk in request.stream():
        if len(body) + len(chunk) > PREVIEW_BODY_LIMIT:
            _preview_refusal(413, 'trial.audio_too_large', 'The recording exceeds the 10 second limit.')
        body.extend(chunk)
    return bytes(body)


def _preview_payload(raw):
    try:
        payload = PreviewPayload.model_validate_json(raw)
    except ValidationError as error:
        audio_error = any((item.get('loc') or (None,))[0] == 'audio' for item in error.errors())
        key = 'trial.invalid_audio' if audio_error else 'trial.invalid_stage'
        status = 400 if audio_error else 422
        message = 'The recording format is invalid.' if audio_error else 'The transcription stage is invalid.'
        _preview_refusal(status, key, message)
    except (ValueError, UnicodeDecodeError):
        _preview_refusal(400, 'trial.invalid_stage', 'The preview request is malformed.')
    if payload.place != 'openai':
        _preview_refusal(422, 'trial.invalid_stage', 'This transcription stage cannot be previewed here.')
    try:
        from ..pipeline.settings import Transcription
        options = payload.options.model_dump(exclude_none=True)
        stage = Transcription(place=payload.place, model=payload.model, options=options)
    except ValidationError:
        _preview_refusal(422, 'trial.invalid_stage', 'The transcription stage is invalid.')
    if payload.audio.encoding != 'pcm_s16le' or payload.audio.sample_rate != 16000:
        _preview_refusal(400, 'trial.invalid_audio', 'The recording must be mono PCM16 at 16 kHz.')
    try:
        pcm = base64.b64decode(payload.audio.data_base64, validate=True)
    except (binascii.Error, ValueError, UnicodeEncodeError):
        _preview_refusal(400, 'trial.invalid_audio', 'The recording format is invalid.')
    if len(pcm) > PREVIEW_PCM_LIMIT:
        _preview_refusal(413, 'trial.audio_too_large', 'The recording exceeds the 10 second limit.')
    if len(pcm) < PREVIEW_PCM_MINIMUM or len(pcm) % 2:
        _preview_refusal(400, 'trial.invalid_audio', 'The recording must be between 250 ms and 10 seconds.')
    return payload, stage, pcm


async def _disconnected(request):
    while True:
        try:
            message = await request.receive()
        except Exception:  # A broken receive channel cannot make a useful preview response either.
            return True
        if message.get('type') == 'http.disconnect':
            return True


async def _transcribe_with_limits(request, transcriber, wav):
    """Cancel provider work on caller disconnect and give it a hard thirty second deadline."""
    provider_task = asyncio.create_task(transcriber.transcribe(wav))
    disconnect_task = asyncio.create_task(_disconnected(request))
    try:
        done, _ = await asyncio.wait({provider_task, disconnect_task}, timeout=PREVIEW_TIMEOUT,
                                     return_when=asyncio.FIRST_COMPLETED)
        if provider_task in done:
            return provider_task.result()
        if disconnect_task in done:
            raise asyncio.CancelledError
        raise asyncio.TimeoutError
    finally:
        for task in (provider_task, disconnect_task):
            if not task.done():
                task.cancel()
        await asyncio.gather(provider_task, disconnect_task, return_exceptions=True)


def mount_models(app):
    from ..control.check_budget import CheckBudget
    budget = app.state.check_budget = CheckBudget()
    preview_budget = app.state.transcription_preview_budget = TranscriptionPreviewBudget()

    @app.get('/api/models/catalog')
    async def model_catalog(request: Request):
        require_same_origin(request)
        return Response(catalog_text(), media_type='application/json')

    @app.post('/api/models/check')
    async def model_check(payload: dict, request: Request):
        """`{stage: 'stt'|'tts', place, model, options, language?}` → `{ok, step, reason?, passes, latency_ms…}`.
        A check that fails is an answer (200, `ok: false`); a request that cannot be checked here is refused:
        the host, which runs no models yet, a device, which checks itself, or a stage that is not valid."""
        require_same_origin(request)
        from ..pipeline import model_check as checks
        from ..pipeline.settings import HOST_UNAVAILABLE, STAGES
        task = payload.get('stage')
        if task not in STAGES:
            raise HTTPException(422, {'key': 'check_invalid', 'message': 'stage must be stt or tts.'})
        if payload.get('place') == 'host':
            raise HTTPException(409, dict(HOST_UNAVAILABLE))
        if payload.get('place') == 'device':
            raise HTTPException(400, dict(CHECK_ON_DEVICE))
        try:
            stage = STAGES[task](place=payload.get('place') or '', model=payload.get('model') or '',
                                 options=payload.get('options') or {})
        except ValidationError as error:
            detail = '; '.join(item.get('msg', '') for item in error.errors()[:3])
            raise HTTPException(422, {'key': 'check_invalid', 'message': detail}) from error
        language = payload.get('language') if isinstance(payload.get('language'), str) else None
        # Bounded: a passed check answers for itself for a while, identical ones share a run, and runs
        # are budgeted per device and per provider. The key is told apart by a digest, never stored or returned.
        from ..control.check_budget import Limited, check_key
        from ..pipeline import integrations
        key = check_key(task, stage.place, stage.model, stage.options, language, integrations.key(stage.place))
        try:
            result = await budget.run(key, device=request.scope.get(DEVICE_KEY) or 'local', provider=stage.place,
                                      work=lambda: checks.check(stage, language=language))
        except Limited as limited:
            raise HTTPException(429, {'key': 'check_rate_limited', 'retry_after': limited.retry_after, 'scope': limited.scope,
                                      'message': f'Too many model checks; try again in {limited.retry_after} s.'},
                                headers={'Retry-After': str(limited.retry_after)}) from limited
        return {'stage': task, 'place': stage.place, 'model': stage.model, **result}

    @app.post('/api/models/transcription/preview')
    async def transcription_preview(request: Request):
        """Transcribe one bounded PCM16 utterance with this node's configured OpenAI key."""
        require_same_origin(request)
        raw = await _bounded_body(request)
        payload, stage, pcm = _preview_payload(raw)
        from ..pipeline import integrations, transcribers
        key = integrations.key('openai')
        if not key:
            _preview_refusal(409, 'trial.provider_unavailable', 'OpenAI transcription is not available on this machine.')
        device = request.scope.get(DEVICE_KEY) or 'local'
        try:
            preview_budget.start(device)
        except PreviewBusy as busy:
            _preview_refusal(429, 'trial.busy', 'A transcription preview is already running or its rate limit was reached.',
                             retry_after=busy.retry_after)

        try:
            from pipecat.audio.utils import pcm_to_wav
            wav = pcm_to_wav(pcm, 16000)
            options = stage.options
            transcriber = transcribers.OpenAITranscriber(
                key, model=stage.model,
                language=options.get('language') if options.get('language') != 'auto' else None,
                prompt=options.get('context'))
            try:
                result = await _transcribe_with_limits(request, transcriber, wav)
            except asyncio.CancelledError:
                raise
            except asyncio.TimeoutError:
                _preview_refusal(504, 'trial.provider_timeout', 'OpenAI transcription took too long.')
            except Exception as error:  # noqa: BLE001 - provider messages and request details must stay private.
                if ((getattr(error, 'status', None) or getattr(error, 'status_code', None)) in (401, 403)
                        or type(error).__name__ in ('AuthenticationError', 'PermissionDeniedError')):
                    _preview_refusal(409, 'trial.provider_unavailable', 'The OpenAI key is unavailable or was refused.')
                _preview_refusal(502, 'trial.provider_failed', 'OpenAI could not transcribe the recording.')

            text = str(getattr(result, 'text', '') or '').strip()
            from ..pipeline.speech_filter import isolated_foreign_script, unreliable_text
            if not text or not any(char.isalnum() for char in text):
                _preview_refusal(422, 'trial.silent', 'No speech was recognized in the recording.')
            language = options.get('language')
            if ((language not in ('hi', 'auto') and isolated_foreign_script(text))
                    or unreliable_text(text, getattr(result, 'confidence', None))):
                _preview_refusal(422, 'trial.unusable', 'The recognized words were not clear enough to use.')
            return {'text': text}
        finally:
            preview_budget.finish(device)
