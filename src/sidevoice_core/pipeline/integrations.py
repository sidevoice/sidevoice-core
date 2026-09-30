"""Integrations: this node's keys for the providers it calls, one per provider whatever it is used for (#64).

A key is the node's, not a pane's and not a client's: an owner writes it from any client, the node keeps it
and calls the provider with it, and no client ever reads it back — only whether there is one, where it came
from, and its last four characters, so a person can tell which key is installed. One file keyed by provider
(mode 0600) holds them. A provider's environment variable is a source too — that is how a headless host is
configured — and a key saved here wins over it. A key nothing uses is kept: which capability a device chooses
is that device's business.
"""
import json
import os
from pathlib import Path

from ..runtime import data_dir

CREDENTIALS = None   # a fixed path overrides the default; see credentials_file()

# What each provider can do for this node, and the environment variable that can supply its key instead. The
# capabilities are what the panes choose among: `transcription` and `voice`.
PROVIDERS = {
    'openai': {'label': 'OpenAI', 'capabilities': ['transcription'], 'environment': 'VOICE_STT_API_KEY'},
    'elevenlabs': {'label': 'ElevenLabs', 'capabilities': ['voice'], 'environment': 'VOICE_ELEVENLABS_API_KEY'},
}


def credentials_file():
    """Where this node keeps its keys, read when asked: `CREDENTIALS` when set (a test), else its data directory."""
    if CREDENTIALS is not None:
        return Path(CREDENTIALS)
    return data_dir() / 'integrations.json'


def _read():
    try:
        data = json.loads(credentials_file().read_text(encoding='utf8'))
        return data if isinstance(data, dict) else {}
    except (OSError, ValueError):
        return {}


def _write(stored):
    target = credentials_file()
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_suffix('.tmp')
    temporary.write_text(json.dumps(stored), encoding='utf8')
    temporary.chmod(0o600)
    temporary.replace(target)


def stored_key(provider):
    value = _read().get(provider)
    return value.strip() if isinstance(value, str) and value.strip() else None


def environment_key(provider, config=None):
    source = config if config is not None else os.environ
    value = (source.get(PROVIDERS[provider]['environment']) or '').strip()
    return value or None


def key(provider, config=None):
    """The key the node calls `provider` with: the saved one, else the environment's, else none."""
    return stored_key(provider) or environment_key(provider, config)


def save_key(provider, value):
    if provider not in PROVIDERS:
        raise ValueError('Proveedor desconocido.')
    value = (value or '').strip()
    if not value:
        raise ValueError('The key is empty.')
    stored = _read()
    stored[provider] = value
    _write(stored)


def clear_key(provider):
    """Remove the saved key. One from the environment stays: it is the deployment's, not the page's to take."""
    stored = _read()
    if stored.pop(provider, None) is None:
        return
    _write(stored)


def credential_state(provider, config=None):
    """Whether `provider` has a key, where from, and its last four characters. Never the key."""
    value, source = stored_key(provider), 'stored'
    if not value:
        value = environment_key(provider, config)
        source = 'environment' if value else None
    return {'configured': bool(value), 'source': source, 'hint': ('…' + value[-4:]) if value else None}


def listing(owner, config=None):
    """The integrations as a client sees them. The owner sees every provider, configured or not, and where
    each key came from. Anyone else sees only the providers they can choose — the configured ones — and
    nothing about the key itself."""
    rows = []
    for provider, meta in PROVIDERS.items():
        state = credential_state(provider, config)
        if not owner and not state['configured']:
            continue
        row = {'id': provider, 'label': meta['label'], 'capabilities': list(meta['capabilities']),
               'configured': state['configured']}
        if owner:
            row.update(source=state['source'], hint=state['hint'], environment=meta['environment'])
        rows.append(row)
    return {'owner': owner, 'providers': rows}
