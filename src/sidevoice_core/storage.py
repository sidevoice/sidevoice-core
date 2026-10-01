"""How this node writes a file that holds a secret. Imports nothing of the package.

Both halves keep one — the control plane its device tokens and its identity, the pipeline the provider keys
it calls with — so the one way to write them sits below both, beside `runtime`, and knows neither.
"""
import os
import secrets
from pathlib import Path


def write_private(path, text):
    """Atomically, and 0600 from its first byte: the file is replaced whole or not at all.

    The temporary file is created exclusively with its final mode, under a name nobody else can be writing to,
    so there is no moment in which another user could read the secret, no moment an interruption could leave it
    readable, and no second writer to trip over."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f'.{path.name}.{os.getpid()}.{secrets.token_hex(4)}.tmp')
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, 'w', encoding='utf8') as file:
            file.write(text)
            file.flush()
            os.fsync(file.fileno())
        os.replace(temporary, path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
