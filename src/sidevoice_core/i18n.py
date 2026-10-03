"""Small message-bundle reader for text the core sends to a person through a client."""
import json
from functools import lru_cache
from importlib.resources import files


@lru_cache(maxsize=None)
def _bundle(language):
    path = files('sidevoice_core.messages').joinpath(f'{language}.json')
    return json.loads(path.read_text(encoding='utf8'))


def translate(key, language='en', **params):
    """Look up a message in the requested language, falling back to the English bundle."""
    english = _bundle('en')
    selected = _bundle(language) if language in {'en', 'es'} else {}
    template = selected.get(key) or english.get(key)
    if not isinstance(template, str):
        return key
    try:
        return template.format(**params)
    except (KeyError, ValueError):
        return template


def keyed_message(key, language='en', **params):
    """A stable client key and parameters with a bundle-derived fallback for older clients."""
    message = {'key': key, 'message': translate(key, language, **params)}
    if params:
        message['params'] = params
    return message
