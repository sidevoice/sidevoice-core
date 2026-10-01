"""Which devices may use this node, and how this node proves it is itself (rubasace/sidevoice
`docs/DEVICE_PAIRING.md`).

- **Identity**: one ECDSA P-256 key per node, created on first start, kept in `node-identity.json` (0600).
  A client pins its public key when it pairs and asks the node to sign a nonce before using a base, so an
  address that is not the node and cannot reach it (a squatter on a stale URL) cannot pass for it. A relay
  that reaches the node can: it forwards the signature, and it sees the pairing secret, the device token and
  all the traffic it carries. The signature encrypts nothing; a relay is trusted with everything that goes
  through it until there is an end-to-end encrypted channel to the pinned identity.
- **Pairing code**: `SV1.` + base64url(JSON) with a one-time secret, valid ten minutes. The secret lives in
  memory only: a code outlives neither its ten minutes nor this process.
- **Devices**: a redeemed secret becomes a device token. Only its SHA-256 is kept (`devices.json`, 0600); a
  revoked device's token stops working at once, because authentication reads the in-memory table the file
  is written from.

Everything here is plain data and a key: what carries it over HTTP is `server.devices`.
"""
import base64
import hashlib
import json
import os
import re
import secrets
import time
import uuid
from collections import OrderedDict
from pathlib import Path

from loguru import logger

from ..storage import write_private

IDENTITY_FILE = 'node-identity.json'
DEVICES_FILE = 'devices.json'
CODE_PREFIX = 'SV1.'
CODE_VERSION = 1
CODE_TTL = 600            # seconds a pairing code can be redeemed
MAX_OUTSTANDING = 5       # codes not yet redeemed; asking for more drops the oldest
LAST_SEEN_EVERY = 60      # seconds: `last_seen` moves (and the file is written) at most this often
IDENTITY_CONTEXT = 'sidevoice-node-identity:'
NAME_LIMIT = 100
NONCE_BYTES = (16, 64)
NONCE_PATTERN = re.compile(r'^[A-Za-z0-9_-]{22,86}={0,2}$')


class IdentityError(ValueError):
    """The identity file exists and is not one: never replaced silently, since every pairing pins it."""


def b64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b'=').decode('ascii')


def b64url_decode(text):
    return base64.urlsafe_b64decode(text + '=' * (-len(text) % 4))


def digest(secret):
    return hashlib.sha256(secret.encode('utf8')).hexdigest()


# ----- the node's identity -----

class NodeIdentity:
    def __init__(self, private_key, created):
        from cryptography.hazmat.primitives import serialization
        self.private_key, self.created = private_key, created
        self.public_der = private_key.public_key().public_bytes(
            serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo)
        # What WebCrypto's importKey('spki', …) takes, and what a client pins.
        self.public_key = base64.b64encode(self.public_der).decode('ascii')
        self.fingerprint = b64url(hashlib.sha256(self.public_der).digest())

    @classmethod
    def load_or_create(cls, path):
        """The node's key, created the first time. Two processes starting at once agree on one key: the
        file is linked into place only if absent, and the loser reads the winner's."""
        path = Path(path)
        try:
            return cls.parse(path.read_text(encoding='utf8'), path)
        except FileNotFoundError:
            pass
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric import ec
        key = ec.generate_private_key(ec.SECP256R1())
        pem = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                                serialization.NoEncryption()).decode('ascii')
        staged = path.with_name(f'.{path.name}.{os.getpid()}.{secrets.token_hex(4)}.new')
        write_private(staged, json.dumps({'private_key_pem': pem, 'created': int(time.time())}))
        try:
            os.link(staged, path)
        except FileExistsError:
            return cls.parse(path.read_text(encoding='utf8'), path)
        finally:
            staged.unlink(missing_ok=True)
        logger.info('This node has a new identity ({})', path)
        return cls.parse(path.read_text(encoding='utf8'), path)

    @classmethod
    def parse(cls, text, path):
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric import ec
        try:
            saved = json.loads(text)
            key = serialization.load_pem_private_key(saved['private_key_pem'].encode('ascii'), password=None)
        except (ValueError, TypeError, KeyError, AttributeError) as error:
            raise IdentityError(f'{path} is not this node\'s identity ({type(error).__name__}). Move it aside to '
                                'create a new one: every paired device will then have to pair again.') from error
        if not isinstance(key, ec.EllipticCurvePrivateKey) or key.curve.name != 'secp256r1':
            raise IdentityError(f'{path} holds a key that is not ECDSA P-256.')
        return cls(key, saved.get('created'))

    def sign(self, nonce):
        """ECDSA P-256 / SHA-256 over `sidevoice-node-identity:<nonce>`, as IEEE P1363 (r‖s), base64url:
        what WebCrypto's `verify({name: 'ECDSA', hash: 'SHA-256'}, …)` takes."""
        from cryptography.hazmat.primitives import hashes
        from cryptography.hazmat.primitives.asymmetric import ec
        from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature
        der = self.private_key.sign((IDENTITY_CONTEXT + nonce).encode('utf8'), ec.ECDSA(hashes.SHA256()))
        r, s = decode_dss_signature(der)
        return b64url(r.to_bytes(32, 'big') + s.to_bytes(32, 'big'))


def valid_nonce(nonce):
    """base64url of 16 to 64 bytes: long enough to be fresh, short enough to be a nonce. What is signed is
    the nonce exactly as sent, so padding, if any, is simply part of it."""
    if not isinstance(nonce, str) or not NONCE_PATTERN.match(nonce):
        return False
    bare = nonce.rstrip('=')
    if len(bare) % 4 == 1:
        return False
    return NONCE_BYTES[0] <= len(b64url_decode(bare)) <= NONCE_BYTES[1]


# ----- the pairing code -----

def encode_code(payload):
    return CODE_PREFIX + b64url(json.dumps(payload, separators=(',', ':'), ensure_ascii=False).encode('utf8'))


def decode_code(code):
    """The payload a code carries. ValueError when it is not one."""
    text = str(code or '').strip()
    if not text.startswith(CODE_PREFIX):
        raise ValueError('Not a Sidevoice pairing code.')
    try:
        payload = json.loads(b64url_decode(text[len(CODE_PREFIX):]).decode('utf8'))
    except (ValueError, UnicodeDecodeError) as error:
        raise ValueError('Not a Sidevoice pairing code.') from error
    if not isinstance(payload, dict):
        raise ValueError('Not a Sidevoice pairing code.')
    return payload


# ----- the devices this node accepts -----

def clean_name(name):
    text = ' '.join(str(name).split())[:NAME_LIMIT] if isinstance(name, str) else ''
    return text or None


class DeviceRegistry:
    """Device tokens by hash, persisted; pairing secrets by hash, in memory. Every method is synchronous and
    the event loop runs one at a time, so a check and the write it leads to never interleave with another."""

    def __init__(self, path, *, clock=time.time):
        self.path = Path(path)
        self.clock = clock
        self.secrets = OrderedDict()   # sha256(secret) -> expires (epoch seconds), oldest first
        self.devices = {}              # id -> {'token_hash', 'name', 'created', 'last_seen'}
        self.by_token = {}             # token_hash -> id
        self.load()

    def load(self):
        try:
            saved = json.loads(self.path.read_text(encoding='utf8'))
        except FileNotFoundError:
            return
        except (OSError, ValueError) as error:
            # Fails closed: no device is accepted until one pairs again.
            logger.warning('{} is unreadable ({}); no device is paired until one pairs again', self.path, type(error).__name__)
            return
        rows = saved.get('devices') if isinstance(saved, dict) else None
        for device_id, entry in (rows.items() if isinstance(rows, dict) else ()):
            if isinstance(entry, dict) and isinstance(entry.get('token_hash'), str) and len(entry['token_hash']) == 64:
                self.devices[device_id] = entry
                self.by_token[entry['token_hash']] = device_id

    def save(self):
        write_private(self.path, json.dumps({'devices': self.devices}, indent=1))

    def issue_secret(self):
        """A one-time secret and when it stops being redeemable."""
        now = int(self.clock())
        for key in [key for key, expires in self.secrets.items() if expires < now]:
            del self.secrets[key]
        secret, expires = secrets.token_urlsafe(16), now + CODE_TTL
        self.secrets[digest(secret)] = expires
        while len(self.secrets) > MAX_OUTSTANDING:
            self.secrets.popitem(last=False)
        return secret, expires

    def redeem(self, secret, name):
        """(device_id, token) for a live secret, consumed; None for an unknown, used or expired one."""
        if not isinstance(secret, str) or not secret:
            return None
        expires = self.secrets.pop(digest(secret), None)
        if expires is None or expires < self.clock():
            return None
        device_id, token, now = str(uuid.uuid4()), secrets.token_urlsafe(32), int(self.clock())
        entry = {'token_hash': digest(token), 'name': clean_name(name), 'created': now, 'last_seen': now}
        self.devices[device_id] = entry
        self.by_token[entry['token_hash']] = device_id
        try:
            self.save()
        except BaseException:
            # Memory and disk say the same thing: a device the file does not hold is not paired.
            del self.devices[device_id], self.by_token[entry['token_hash']]
            raise
        return device_id, token

    def authenticate(self, token):
        """The device this token belongs to, or None."""
        if not isinstance(token, str) or not token:
            return None
        device_id = self.by_token.get(digest(token))
        if device_id is None:
            return None
        entry, now = self.devices[device_id], int(self.clock())
        if now - (entry.get('last_seen') or 0) >= LAST_SEEN_EVERY:
            entry['last_seen'] = now
            try:
                self.save()
            except OSError as error:
                logger.warning('Could not record when a device was last seen: {}', error)
        return device_id

    def listing(self, current=None):
        return [{'id': device_id, 'name': entry.get('name'), 'created': entry.get('created'),
                 'last_seen': entry.get('last_seen'), 'current': device_id == current}
                for device_id, entry in sorted(self.devices.items(), key=lambda item: item[1].get('created') or 0)]

    def revoke(self, device_id):
        """Its token stops working now; the file follows. False when no such device is paired."""
        entry = self.devices.pop(device_id, None) if isinstance(device_id, str) else None
        if entry is None:
            return False
        self.by_token.pop(entry['token_hash'], None)
        self.save()
        return True


class NodeDevices:
    """The node's identity and its devices, in its data directory. Both are read when first needed: a
    process that never pairs anything never touches either file."""

    def __init__(self, directory, *, clock=time.time):
        self.directory = Path(directory)
        self.clock = clock
        self._identity = self._registry = None

    @property
    def identity(self):
        if self._identity is None:
            self._identity = NodeIdentity.load_or_create(self.directory / IDENTITY_FILE)
        return self._identity

    @property
    def registry(self):
        if self._registry is None:
            self._registry = DeviceRegistry(self.directory / DEVICES_FILE, clock=self.clock)
        return self._registry

    def issue_code(self, *, host, urls, rv):
        """A pairing code, its payload (keys exactly the contract's; absent values null) and its lifetime."""
        secret, expires = self.registry.issue_secret()
        payload = {'v': CODE_VERSION, 'fp': self.identity.fingerprint, 'host': host or None, 'urls': list(urls),
                   'rv': rv or None, 'secret': secret, 'exp': expires}
        return {'code': encode_code(payload), 'payload': payload, 'expires_in': CODE_TTL}
