"""Speech-to-text catalogue for browser-local and OpenAI cloud transcription."""
import json
import os
import re
from pathlib import Path
from .paths import RUNTIME_ROOT

import aiohttp

CREDENTIALS = Path(os.getenv('VOICE_STT_CREDENTIALS_FILE', str(RUNTIME_ROOT / 'stt-credentials.json')))

BROWSER_MODELS = [
    {'id': 'onnx-community/whisper-tiny', 'label': 'Whisper tiny',
     'description': 'Fastest and lightest; recommended for conversation and mobile.',
     'devices': ['webgpu', 'wasm']},
    {'id': 'onnx-community/whisper-base', 'label': 'Whisper base',
     'description': 'More accurate, with a larger download and more latency.',
     'devices': ['webgpu', 'wasm']},
    {'id': 'onnx-community/whisper-small', 'label': 'Whisper small',
     'description': 'Mejor calidad multilingüe. Aproximadamente 285 MiB en Q4; requiere WebGPU.',
     'devices': ['webgpu']},
    {'id': 'onnx-community/whisper-large-v3-turbo', 'label': 'Whisper large v3 turbo',
     'description': 'Best local quality available. About 538 MiB quantised; needs WebGPU with fp16.',
     'devices': ['webgpu']},
]
OPENAI_API = 'https://api.openai.com'
DEFAULT_OPENAI_MODEL = 'gpt-4o-transcribe'
MODEL_ID = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._:-]{0,119}$')
CATALOG = {
    'providers': [
        {'id': 'browser', 'label': 'In this browser', 'needs_key': False,
         'note': 'The room receives the audio to detect your turns; the text is recognised in this browser, with no API. WebGPU uses the GPU; CPU uses WebAssembly.',
         'default_model': 'onnx-community/whisper-tiny', 'models': BROWSER_MODELS},
        {'id': 'openai', 'label': 'OpenAI', 'needs_key': True,
         'note': 'The audio of your turns is sent to OpenAI for transcription.',
         'default_model': DEFAULT_OPENAI_MODEL, 'models': [], 'models_source': 'remote'},
    ],
}
PROVIDERS = {item['id']: item for item in CATALOG['providers']}


def _read():
    try:
        stored = json.loads(CREDENTIALS.read_text())
        return stored if isinstance(stored, dict) else {}
    except (OSError, ValueError):
        return {}


def stored_key(provider):
    value = _read().get(provider)
    return value if isinstance(value, str) and value.strip() else None


def environment_key(config=None):
    source = config if config is not None else os.environ
    value = (source.get('VOICE_STT_API_KEY') or '').strip()
    return value or None


def save_key(provider, key):
    if provider not in PROVIDERS or not PROVIDERS[provider]['needs_key']:
        raise ValueError('Ese proveedor no usa clave')
    key = (key or '').strip()
    if not key:
        raise ValueError('The key is empty')
    stored = _read()
    stored[provider] = key
    CREDENTIALS.parent.mkdir(parents=True, exist_ok=True)
    temporary = CREDENTIALS.with_suffix('.tmp')
    temporary.write_text(json.dumps(stored), encoding='utf8')
    temporary.chmod(0o600)
    temporary.replace(CREDENTIALS)


def clear_key(provider):
    stored = _read()
    if stored.pop(provider, None) is None:
        return
    CREDENTIALS.parent.mkdir(parents=True, exist_ok=True)
    temporary = CREDENTIALS.with_suffix('.tmp')
    temporary.write_text(json.dumps(stored), encoding='utf8')
    temporary.chmod(0o600)
    temporary.replace(CREDENTIALS)


def credential_state(config=None):
    state = {}
    for provider in PROVIDERS.values():
        if not provider['needs_key']:
            continue
        key = stored_key(provider['id'])
        source = 'stored' if key else None
        if not key and provider['id'] == 'openai':
            key = environment_key(config)
            source = 'environment' if key else None
        state[provider['id']] = {'configured': bool(key), 'source': source,
                                 'hint': ('…' + key[-4:]) if key else None}
    return state


def resolve(settings, config=None):
    provider = getattr(settings, 'stt_provider', 'browser') or 'browser'
    if provider not in PROVIDERS:
        provider = 'browser'
    model = (getattr(settings, 'stt_model', '') or '').strip()
    if provider == 'browser':
        known = {item['id'] for item in PROVIDERS[provider]['models']}
        if model not in known:
            model = PROVIDERS[provider]['default_model']
    elif not MODEL_ID.fullmatch(model) or model.startswith('onnx-community/'):
        # Cloud catalogues change independently of Sidevoice releases. Keep a
        # valid account-provided model instead of pinning it to a baked list.
        model = PROVIDERS[provider]['default_model']
    if provider == 'browser':
        return {'provider': provider, 'model': model, 'reason': 'explicit',
                'engine': 'Transformers.js · Whisper', 'location': 'browser',
                'device': getattr(settings, 'stt_device', 'auto'),
                'compute_type': 'fp32 (WebGPU) / q8 (WASM)'}
    key = stored_key('openai') or environment_key(config)
    return {'provider': provider, 'model': model,
            'reason': 'explicit' if key else 'missing_key',
            'engine': 'OpenAI API', 'location': 'remote', 'device': 'cloud',
            'compute_type': None, 'available': bool(key)}


def build(settings, choice, *, config=None, send=None, session_id=None):
    """The transcription provider for one call: OpenAI from here, or the browser that is speaking."""
    language = None if settings.stt_language == 'auto' else settings.stt_language
    if choice['provider'] == 'browser':
        if send is None or not session_id:
            raise ValueError('Browser transcription needs its own connection.')
        from .transcribers import BrowserTranscriber
        return BrowserTranscriber(send, session_id, language=language)
    key = stored_key('openai') or environment_key(config)
    if not key:
        raise ValueError('OpenAI necesita una clave de API antes de conectar.')
    from .transcribers import OpenAITranscriber
    return OpenAITranscriber(key, model=choice['model'], language=language, prompt=settings.stt_context or None)


async def verify(provider, key):
    if provider != 'openai':
        return
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=10)) as http:
            async with http.get(OPENAI_API + '/v1/models',
                                headers={'Authorization': 'Bearer ' + key}) as response:
                if response.status == 401:
                    raise ValueError('OpenAI rejected the key.')
                if response.status >= 400:
                    raise ValueError(f'OpenAI answered {response.status} when checking the key.')
    except ValueError:
        raise
    except Exception as error:
        raise ValueError('No se pudo comprobar la clave con OpenAI: ' + type(error).__name__) from error


def _is_transcription_model(model_id):
    """Recognise transcription IDs because /v1/models exposes no capabilities."""
    return model_id == 'whisper-1' or ('transcribe' in model_id and not any(marker in model_id for marker in ('realtime', 'live')))


async def _models(http, value):
    async with http.get(OPENAI_API + '/v1/models',
                        headers={'Authorization': 'Bearer ' + value}) as response:
        if response.status in {401, 403}:
            raise ValueError('OpenAI rejected the key.')
        if response.status >= 400:
            raise ValueError(f'OpenAI answered {response.status} when loading the models.')
        payload = await response.json()
    models = []
    for item in payload.get('data', []) if isinstance(payload, dict) else []:
        model_id = item.get('id') if isinstance(item, dict) else None
        if isinstance(model_id, str) and MODEL_ID.fullmatch(model_id) and _is_transcription_model(model_id):
            models.append({'id': model_id, 'label': model_id})
    return sorted(models, key=lambda item: (item['id'] != DEFAULT_OPENAI_MODEL, item['id']))


async def catalog(provider, config=None):
    """Load a provider model catalogue only when its picker is selected."""
    if provider != 'openai':
        raise ValueError('Proveedor desconocido.')
    value = stored_key(provider) or environment_key(config)
    result = {'provider': provider, 'configured': bool(value), 'models': [], 'error': None}
    if not value:
        return result
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=12)) as http:
            result['models'] = await _models(http, value)
        if not result['models']:
            result['error'] = 'OpenAI returned no transcription models for this account.'
    except ValueError as error:
        result['error'] = str(error)
    except Exception as error:
        result['error'] = 'Could not load the OpenAI catalogue: ' + type(error).__name__
    return result
