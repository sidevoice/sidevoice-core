"""How this node writes a file that holds a secret, and checks the directory it keeps them in. Imports nothing
of the package.

Both halves keep one — the control plane its device tokens and its identity, the pipeline the provider keys
it calls with — so the one way to write them sits below both, beside `runtime`, and knows neither.
"""
import os
import secrets
import stat
from pathlib import Path


def unsafe_directory(path):
    """Why `path` cannot hold this node's secrets, or None when it can: a directory itself (not a link to one),
    owned by this user, that no group and no other user may enter. The connector creates it so; this checks what is
    there rather than trusting that it was, since everything inside — the local socket, the connector's credential,
    the node's key — is only as private as the directory around it."""
    try:
        found = os.lstat(path)
    except OSError as error:
        return f'{path} cannot be read ({error.strerror or type(error).__name__}).'
    if not stat.S_ISDIR(found.st_mode):
        return f'{path} is not a directory.'
    if found.st_uid != os.getuid():
        return f'{path} belongs to another user (uid {found.st_uid}).'
    if stat.S_IMODE(found.st_mode) & 0o077:
        return f'{path} is open to other users (mode {stat.S_IMODE(found.st_mode):04o}); it must be 0700.'
    return None


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


def write_private_into(directory, name, text):
    """`write_private`, into `directory` only if it is one of this user's own, reached without following a link at
    its last step: a directory refused as a link to somewhere else, or as someone else's, is never written through.
    The file is created and replaced relative to the directory opened and checked, never by its path again.
    OSError when it is not such a directory."""
    folder = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        if os.fstat(folder).st_uid != os.getuid():
            raise PermissionError(f'{directory} belongs to another user')
        temporary = f'.{name}.{os.getpid()}.{secrets.token_hex(4)}.tmp'
        descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=folder)
        try:
            os.fchmod(descriptor, 0o600)
            with os.fdopen(descriptor, 'w', encoding='utf8') as file:
                file.write(text)
                file.flush()
                os.fsync(file.fileno())
            os.replace(temporary, name, src_dir_fd=folder, dst_dir_fd=folder)
        except BaseException:
            try:
                os.unlink(temporary, dir_fd=folder)
            except OSError:
                pass
            raise
    finally:
        os.close(folder)
