"""Speech to text, by where the stage is placed: the client transcribes itself (`device`: its own Whisper, in a
page or in the desktop app's native engine), a provider transcribes from this node with the node's key
(`openai`), or the host — which runs no models yet, and is refused before a call is built on it."""
from . import integrations
from .settings import MODEL_ID

OPENAI_API = 'https://api.openai.com'
DEFAULT_OPENAI_MODEL = 'gpt-4o-transcribe'


def resolve(settings, config=None):
    """What one call transcribes with: where, which model, in which language (None to detect it), with which
    context, and whether it can run at all — a provider needs its key here, the host is not available yet."""
    stage = settings.stt
    language = stage.options.get('language')
    choice = {'place': stage.place, 'model': stage.model,
              'language': None if language in (None, 'auto') else language,
              'context': stage.options.get('context') or ''}
    if stage.place == 'device':
        available = True
    elif stage.place == 'host':
        available = False
    else:
        available = bool(integrations.key(stage.place, config))
    return {**choice, 'available': available}


def build(choice, *, config=None, send=None, session_id=None):
    """The transcription provider for one call: the client that is speaking, or a provider from here."""
    if choice['place'] == 'device':
        if send is None or not session_id:
            raise ValueError('Transcription on the device needs its own connection.')
        from .transcribers import ClientTranscriber
        return ClientTranscriber(send, session_id, language=choice['language'])
    if choice['place'] == 'openai':
        key = integrations.key('openai', config)
        if not key:
            raise ValueError('OpenAI needs an API key before connecting.')
        from .transcribers import OpenAITranscriber
        return OpenAITranscriber(key, model=choice['model'], language=choice['language'],
                                 prompt=choice['context'] or None)
    raise ValueError(f'Nothing transcribes on {choice["place"]!r} yet.')


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
        raise ValueError('Unknown provider.')
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
