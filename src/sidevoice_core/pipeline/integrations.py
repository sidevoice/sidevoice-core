"""Integrations: this node's keys for the providers it calls, one per provider whatever it is used for.

A key is the node's, not a pane's and not a client's: any paired device writes it, the node keeps it
and calls the provider with it, and no client ever reads it back — only whether there is one, where it came
from, and its last four characters, so a person can tell which key is installed. One file keyed by provider
(mode 0600) holds them. A provider's environment variable is a source too — that is how a headless host is
configured — and a key saved here wins over it. A key nothing uses is kept: which capability a device chooses
is that device's business.

Writes are ordered by intent, not by arrival: a key is verified with its provider before it is saved, which takes
a network round trip, so every change to a provider's key takes a ticket first (`ticket`), and a save whose ticket
a later change has superseded — a newer key, or a removal — stores nothing (`Superseded`). A removal can therefore
never be undone by a save that was still being verified when it happened, whichever client sent either.
"""
import json
import os
from pathlib import Path

from ..runtime import data_dir
from ..storage import write_private

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
    write_private(credentials_file(), json.dumps(stored))


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


class Superseded(Exception):
    """A later change to the same provider's key was asked for while this one was still being verified."""


# The last ticket handed out per (keys file, provider). In memory: a ticket only has to outlive the request that
# holds it, and one process serves every request to this node.
_generations = {}


def _slot(provider):
    return str(credentials_file()), provider


def ticket(provider):
    """Announce a change to `provider`'s key before doing the slow part of it; every earlier ticket is void."""
    slot = _slot(provider)
    _generations[slot] = _generations.get(slot, 0) + 1
    return _generations[slot]


def save_key(provider, value, ticket=None):
    """Store `provider`'s key. With a `ticket`, only if no change was asked for since it was taken: checked and
    written with nothing awaited in between, so no other request can slip in."""
    if provider not in PROVIDERS:
        raise ValueError('Unknown provider.')
    value = (value or '').strip()
    if not value:
        raise ValueError('The key is empty.')
    if ticket is not None and ticket != _generations.get(_slot(provider)):
        raise Superseded(provider)
    stored = _read()
    stored[provider] = value
    _write(stored)


def clear_key(provider):
    """Remove the saved key, and void any save still being verified. One from the environment stays: it is the
    deployment's, not the page's to take."""
    ticket(provider)
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


def listing(config=None):
    """The integrations as every paired device sees them: each provider, configured or not, where its key came
    from and its last four characters — never the key. A paired device has the node's full authority (by
    design: no owner and no guests), so there is no second, narrower listing."""
    rows = []
    for provider, meta in PROVIDERS.items():
        state = credential_state(provider, config)
        rows.append({'id': provider, 'label': meta['label'], 'capabilities': list(meta['capabilities']),
                     'configured': state['configured'], 'source': state['source'], 'hint': state['hint'],
                     'environment': meta['environment']})
    return {'providers': rows}
