"""Where this node keeps its files, and what it is. Imports nothing of the package.

Both halves read it — the pipeline for the provider keys it calls with, the control plane for the
pairings it keeps — so it sits below both and knows neither. Everything is read when asked, never
at import: a test, and a second core in one process, get the environment they were given.
"""
import os
from pathlib import Path


def data_dir(environ=None):
    """The node's own directory: provider keys, the local link's credential, the ready file.

    `SIDEVOICE_CORE_DATA_DIR` names it; otherwise it sits beside the connector's own files, under
    `~/.sidevoice`.
    """
    environ = os.environ if environ is None else environ
    named = environ.get('SIDEVOICE_CORE_DATA_DIR')
    return Path(named) if named else Path.home() / '.sidevoice' / 'core'


def version():
    try:
        from importlib.metadata import version as installed
        return installed('sidevoice-core')
    except Exception:
        return None


def build_info():
    """What this core is, for a page to compare with itself. The web build is whoever serves the page's."""
    return {'version': version()}
