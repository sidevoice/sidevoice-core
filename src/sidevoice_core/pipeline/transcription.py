"""Speech-to-text catalogue: the client transcribes (its own Whisper), or OpenAI from this node."""
import re

from . import integrations

# 'native': the same model run by the client's own engine outside the page (the desktop app, which downloads it
# once); a client offers it only when it has such an engine.
BROWSER_MODELS = [
    {'id': 'onnx-community/whisper-tiny', 'label': 'Whisper tiny',
     'description': 'Fastest and lightest; recommended for conversation and mobile.',
     'devices': ['webgpu', 'wasm', 'native']},
    {'id': 'onnx-community/whisper-base', 'label': 'Whisper base',
     'description': 'More accurate, with a larger download and more latency.',
     'devices': ['webgpu', 'wasm', 'native']},
    {'id': 'onnx-community/whisper-small', 'label': 'Whisper small',
     'description': 'Mejor calidad multilingüe. Aproximadamente 285 MiB en Q4; requiere WebGPU.',
     'devices': ['webgpu', 'native']},
    {'id': 'onnx-community/whisper-large-v3-turbo', 'label': 'Whisper large v3 turbo',
     'description': 'Best local quality available. About 538 MiB quantised; needs WebGPU with fp16.',
     'devices': ['webgpu', 'native']},
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
    key = integrations.key('openai', config)
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
        from .transcribers import ClientTranscriber
        return ClientTranscriber(send, session_id, language=language)
    key = integrations.key('openai', config)
    if not key:
        raise ValueError('OpenAI necesita una clave de API antes de conectar.')
    from .transcribers import OpenAITranscriber
    return OpenAITranscriber(key, model=choice['model'], language=language, prompt=settings.stt_context or None)


async def verify(provider, key):
    if provider != 'openai':
        return
    import aiohttp
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
    value = integrations.key(provider, config)
    result = {'provider': provider, 'configured': bool(value), 'models': [], 'error': None}
    if not value:
        return result
    import aiohttp
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
