"""Where a credential may travel in clear.

A pairing secret, a device token, a connector credential or a dial key travels only over TLS, with two
exceptions: loopback, which never leaves the machine, and the hosts named in `SIDEVOICE_TRUSTED_CLUSTER_HOSTS`.

`SIDEVOICE_TRUSTED_CLUSTER_HOSTS` is comma-separated. An entry starting with a dot is a suffix
(`.svc.cluster.local` matches `room.voice.svc.cluster.local`); any other entry is one exact host name. It is
empty by default. Naming a host there is the operator's statement that the network between this node and that
host is theirs (a Kubernetes cluster's pod network, say) and that anyone on it may read what crosses it: a host
name alone proves nothing about where it resolves or how the packets are routed, so no spelling is trusted by
default.
"""
import os
from urllib.parse import urlsplit

LOOPBACK = frozenset({'127.0.0.1', 'localhost', '::1', '[::1]'})
TRUSTED_CLUSTER_HOSTS = 'SIDEVOICE_TRUSTED_CLUSTER_HOSTS'


def trusted_cluster_hosts(environ=None):
    """The configured entries, lower-cased: exact host names, and suffixes starting with a dot."""
    environ = os.environ if environ is None else environ
    return tuple(entry.strip().lower() for entry in str(environ.get(TRUSTED_CLUSTER_HOSTS) or '').split(',')
                 if entry.strip().strip('.'))


def plaintext_allowed(hostname, environ=None):
    """Whether a credential may reach `hostname` without TLS: loopback, or a configured trusted cluster host."""
    host = (hostname or '').lower().rstrip('.')
    if not host:
        return False
    if host in LOOPBACK:
        return True
    return any(host.endswith(entry) if entry.startswith('.') else host == entry
               for entry in trusted_cluster_hosts(environ))


def credential_safe(url, environ=None):
    """Whether `url` may be handed a credential: https (or wss) anywhere, http (or ws) only where
    `plaintext_allowed` says so. Anything else, a URL without a host included, is not."""
    try:
        parts = urlsplit(str(url))
        hostname = parts.hostname
    except ValueError:
        return False
    if not hostname:
        return False
    if parts.scheme in {'https', 'wss'}:
        return True
    return parts.scheme in {'http', 'ws'} and plaintext_allowed(hostname, environ)
