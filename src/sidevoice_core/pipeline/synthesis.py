"""Speech-synthesis providers and their server-side credentials.

Kokoro remains a browser-only provider. ElevenLabs is deliberately invoked by
the room server: its key never reaches the browser, while the generated audio
does. The account catalogue is fetched on demand so newly created, cloned, or
workspace voices appear without a Sidevoice release.
"""
import asyncio
import base64
import json
import os
import time
from pathlib import Path
from urllib.parse import quote

import aiohttp
from .paths import RUNTIME_ROOT

CREDENTIALS = Path(os.getenv('VOICE_TTS_CREDENTIALS_FILE', str(RUNTIME_ROOT / 'tts-credentials.json')))
ELEVENLABS_API = 'https://api.elevenlabs.io'

FALLBACK_MODELS = [
    {'id': 'eleven_flash_v2_5', 'label': 'Eleven Flash v2.5', 'description': 'Fast'},
    {'id': 'eleven_multilingual_v2', 'label': 'Eleven Multilingual v2', 'description': 'Calidad multilingüe'},
    {'id': 'eleven_v3', 'label': 'Eleven v3', 'description': 'More expressive'},
]


def _read():
    try:
        data = json.loads(CREDENTIALS.read_text(encoding='utf8'))
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def stored_key():
    value = _read().get('elevenlabs')
    return value.strip() if isinstance(value, str) and value.strip() else None


def environment_key(config=None):
    source = config if config is not None else os.environ
    value = (source.get('VOICE_ELEVENLABS_API_KEY') or '').strip()
    return value or None


def key(config=None):
    return stored_key() or environment_key(config)


def credential_state(config=None):
    value = stored_key()
    source = 'stored' if value else None
    if not value:
        value = environment_key(config)
        source = 'environment' if value else None
    return {'configured': bool(value), 'source': source,
            'hint': ('…' + value[-4:]) if value else None}


def save_key(value):
    value = (value or '').strip()
    if not value:
        raise ValueError('The key is empty.')
    stored = _read()
    stored['elevenlabs'] = value
    CREDENTIALS.parent.mkdir(parents=True, exist_ok=True)
    temporary = CREDENTIALS.with_suffix('.tmp')
    temporary.write_text(json.dumps(stored), encoding='utf8')
    temporary.chmod(0o600)
    temporary.replace(CREDENTIALS)


def clear_key():
    stored = _read()
    if stored.pop('elevenlabs', None) is None:
        return
    CREDENTIALS.parent.mkdir(parents=True, exist_ok=True)
    temporary = CREDENTIALS.with_suffix('.tmp')
    temporary.write_text(json.dumps(stored), encoding='utf8')
    temporary.chmod(0o600)
    temporary.replace(CREDENTIALS)


def _headers(value):
    return {'xi-api-key': value, 'Accept': 'application/json'}


def _failure(response, provider='ElevenLabs'):
    if response.status in {401, 403}:
        return f'{provider} rejected the key.'
    return f'{provider} answered {response.status}.'


async def verify(value):
    """Validate the non-billable capabilities this integration actually needs.

    Subscription details are deliberately not used as a health check: a
    restricted service key may not have that administrative scope. Listing TTS
    models and account voices is required for Sidevoice's model picker and its
    custom-voice picker, and performs no synthesis.
    """
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=10)) as http:
            await _models(http, value)
            try:
                await _voices(http, value)
            except ValueError as error:
                raise ValueError('La clave no puede leer voces de ElevenLabs. Activa el permiso de voces (y Text to Speech) para usar voces personales o clonadas.') from error
    except ValueError:
        raise
    except Exception as error:
        raise ValueError('No se pudo comprobar la clave con ElevenLabs: ' + type(error).__name__) from error


async def _models(http, value):
    async with http.get(ELEVENLABS_API + '/v1/models', headers=_headers(value)) as response:
        if response.status >= 400:
            raise ValueError(_failure(response))
        payload = await response.json()
    models = []
    for item in payload if isinstance(payload, list) else []:
        if not item.get('can_do_text_to_speech'):
            continue
        model_id = item.get('model_id')
        if isinstance(model_id, str) and model_id:
            name = item.get('name') or model_id
            description = item.get('description')
            entry = {'id': model_id, 'label': name}
            if isinstance(description, str) and description.strip():
                entry['description'] = description.strip()
            models.append(entry)
    return models


async def _voices(http, value):
    voices, page_token = [], None
    for _ in range(20):  # Protect the settings endpoint from a malformed pagination response.
        query = '?page_size=100' + (('&next_page_token=' + quote(page_token)) if page_token else '')
        async with http.get(ELEVENLABS_API + '/v2/voices' + query, headers=_headers(value)) as response:
            if response.status >= 400:
                # Accounts on an older API surface still expose the legacy list endpoint.
                if response.status == 404 and not page_token:
                    async with http.get(ELEVENLABS_API + '/v1/voices', headers=_headers(value)) as legacy:
                        if legacy.status >= 400:
                            raise ValueError(_failure(legacy))
                        payload = await legacy.json()
                    return payload.get('voices', []) if isinstance(payload, dict) else []
                raise ValueError(_failure(response))
            payload = await response.json()
        entries = payload.get('voices', []) if isinstance(payload, dict) else []
        voices.extend(entry for entry in entries if isinstance(entry, dict))
        page_token = payload.get('next_page_token') if isinstance(payload, dict) else None
        if not payload.get('has_more') or not page_token:
            break
    return voices


def _language_code(value):
    if not isinstance(value, str) or not value.strip():
        return None
    code = value.strip().lower().replace('_', '-').split('-', 1)[0]
    return code if code.isalpha() and 2 <= len(code) <= 3 else None


def _voice_languages(item):
    # Prefer the voice's native/primary language. A multilingual voice can have
    # many verified previews, but that does not make every accent appropriate
    # as the default choice for every language.
    labels = item.get('labels') if isinstance(item.get('labels'), dict) else {}
    primary = _language_code(labels.get('language') or item.get('language'))
    if primary:
        return [primary]
    verified = item.get('verified_languages')
    languages = {
        code for entry in verified if isinstance(entry, dict)
        if (code := _language_code(entry.get('language')))
    } if isinstance(verified, list) else set()
    return sorted(languages)


def _voice_entry(item):
    voice_id = item.get('voice_id')
    if not isinstance(voice_id, str) or not voice_id:
        return None
    name = item.get('name') or voice_id
    category = item.get('category') or item.get('voice_type')
    return {'id': voice_id, 'label': name, 'description': category,
            'languages': _voice_languages(item)}


async def catalog(config=None):
    """Return account-aware models and voices, degrading gracefully while offline."""
    state = credential_state(config)
    result = {'configured': state['configured'], 'models': FALLBACK_MODELS, 'voices': [], 'error': None}
    value = key(config)
    if not value:
        return result
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=12)) as http:
            models, voices = await asyncio.gather(_models(http, value), _voices(http, value))
        if models:
            result['models'] = models
        result['voices'] = [entry for item in voices if (entry := _voice_entry(item))]
    except ValueError as error:
        result['error'] = str(error)
    except Exception as error:
        result['error'] = 'Could not load the ElevenLabs catalogue: ' + type(error).__name__
    return result


async def synthesize(text, *, model, voice, speed, config=None, with_timestamps=False):
    """Generate one MP3 response through ElevenLabs without exposing its key."""
    value = key(config)
    if not value:
        raise ValueError('Configura una clave de ElevenLabs antes de seleccionar esa voz.')
    if not voice:
        raise ValueError('Elige una voz de ElevenLabs.')
    body = {'text': text, 'model_id': model,
            'voice_settings': {'speed': max(0.7, min(1.2, float(speed)))}}
    endpoint = '/with-timestamps' if with_timestamps else '/stream'
    url = ELEVENLABS_API + '/v1/text-to-speech/' + quote(voice, safe='') + endpoint + '?output_format=mp3_44100_128'
    started = time.monotonic()
    timings = {}
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=45)) as http:
            async with http.post(url, headers={**_headers(value), 'Content-Type': 'application/json'}, json=body) as response:
                timings['request_to_headers_ms'] = (time.monotonic() - started) * 1000
                if response.status >= 400:
                    raise ValueError(_failure(response))
                chunks = []
                async for chunk in response.content.iter_any():
                    if chunk:
                        if not chunks:
                            timings['request_to_first_chunk_ms'] = (time.monotonic() - started) * 1000
                        chunks.append(chunk)
                audio = b''.join(chunks)
                timings['request_to_complete_ms'] = (time.monotonic() - started) * 1000
    except ValueError:
        raise
    except Exception as error:
        raise ValueError('No se pudo generar audio con ElevenLabs: ' + type(error).__name__) from error
    alignment = None
    if with_timestamps:
        try:
            result = json.loads(audio)
            audio = base64.b64decode(result['audio_base64'], validate=True)
            candidate = result.get('alignment')
            if isinstance(candidate, dict):
                alignment = {field: candidate.get(field) for field in
                             ('characters', 'character_start_times_seconds', 'character_end_times_seconds')}
        except (ValueError, KeyError, TypeError) as error:
            raise ValueError('ElevenLabs returned an invalid audio response.') from error
    if not audio:
        raise ValueError('ElevenLabs returned no audio.')
    return {'mime_type': 'audio/mpeg', 'audio_base64': base64.b64encode(audio).decode('ascii'),
            'timings_ms': timings, 'alignment': alignment}
