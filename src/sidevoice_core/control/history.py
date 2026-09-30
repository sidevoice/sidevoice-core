"""The room's journal, in memory, and the little that must outlive the process, in one small file.

Nothing anyone says is written to disk by the room: transcripts, the outbox and
the state of every spoken reply live only while the room runs (issue #1). What
survives a restart or a redeploy is what would otherwise have to be redone by
hand: connector credentials, one pairing per machine (issue #12). Bindings are
not kept: every connector re-registers its own on reconnect, and closing a
conversation's voice from the room simply removes its binding.
"""
import hashlib
import json
import os
import secrets
import sqlite3
import time
import uuid
from collections import OrderedDict
from pathlib import Path

# Seconds before the next delivery attempt after the n-th failure; the last value repeats.
RETRY_BACKOFF = (2, 5, 15, 60)
# How long the room keeps holding a message for a conversation that is not there. A connector that
# drops and comes back inside this window loses nothing, which is what the outbox is for; past it the
# sentence has stopped being something to answer and become something said long ago to somebody who
# was not listening, so it is never delivered (#41).
PENDING_TTL = int(os.environ.get('VOICE_INPUT_TTL_SECONDS') or 600)
HISTORY_KEYS = ('seq', 'id', 'thread', 'role', 'text', 'name', 'session', 'revision', 'time', 'status',
                'audio_reason', 'offline')
# What a machine says about itself, and how much of it is kept. `harnesses` is a list; the rest is text.
IDENTITY_KEYS = ('host', 'platform', 'version', 'harnesses')
IDENTITY_LIMITS = {'host': 200, 'platform': 60, 'version': 40}


def _hash(token):
    return hashlib.sha256(token.encode()).hexdigest()


def connector_identity(value):
    """How a machine describes itself, kept to what it actually said. Nothing is inferred and nothing
    is invented: a field the connector did not send is simply not written, so an older client's row
    keeps saying what it said last time instead of losing it to an empty string."""
    said = value if isinstance(value, dict) else {}
    identity = {}
    for key, limit in IDENTITY_LIMITS.items():
        text = said.get(key)
        if isinstance(text, str) and text.strip():
            identity[key] = text.strip()[:limit]
    harnesses = said.get('harnesses')
    if isinstance(harnesses, (list, tuple)):
        identity['harnesses'] = [name.strip()[:40] for name in harnesses[:8] if isinstance(name, str) and name.strip()]
    return identity


class RoomHistory:
    MAX_MESSAGES = 2000

    def __init__(self, path):
        # `path` names where the room keeps its durable state; a legacy database next to it is read once.
        self.path = Path(path)
        self.state_path = self.path if self.path.suffix == '.json' else self.path.with_name('room-state.json')
        self.messages = OrderedDict()   # id -> row
        self.seq = 0
        self._bindings = {}             # id -> binding
        self.pairing_codes = {}         # code -> {'expires', 'redeemed'}
        self.connectors = {}            # id -> {'token_hash', 'created', 'last_seen', 'revoked', **identity}
        self._load_state()

    # ----- the durable file -----

    def _load_state(self):
        try:
            data = json.loads(self.state_path.read_text())
        except (OSError, ValueError):
            data = None
        if isinstance(data, dict):
            self.connectors = {k: v for k, v in (data.get('connectors') or {}).items() if isinstance(v, dict)}
            return
        self._import_legacy()

    def _import_legacy(self):
        """A room that kept a database gets its pairings back, once; the database is not read again."""
        legacy = self.state_path.with_name('room-history.sqlite3')
        if not legacy.exists():
            return
        try:
            db = sqlite3.connect(f'file:{legacy}?mode=ro', uri=True)
            db.row_factory = sqlite3.Row
            try:
                for row in db.execute('SELECT id, token_hash, host, created, last_seen, revoked FROM connectors'):
                    self.connectors[row['id']] = {'token_hash': row['token_hash'], 'host': row['host'], 'created': row['created'],
                                                  'last_seen': row['last_seen'], 'revoked': int(row['revoked'] or 0)}
            finally:
                db.close()
        except sqlite3.Error:
            return
        self._save_state()

    def _save_state(self):
        self.state_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.state_path.with_suffix('.tmp')
        temporary.write_text(json.dumps({'connectors': self.connectors}, indent=1))
        temporary.chmod(0o600)
        os.replace(temporary, self.state_path)

    # ----- transcript and outbox (memory only) -----

    def put(self, *, id, thread, role, text, name, session, revision, status, language=None, payload=None,
            offline=None, at=None):
        """`at` is the clock of whoever produced the message, used for input the room did not hear as it
        happened; `offline` says that it reached the room after the fact, so the transcript can say so too."""
        previous = self.messages.get(id)
        if previous:
            if (previous['thread'], previous['text'], previous['revision'], previous['language']) != (thread, text, revision, language):
                raise ValueError('Message identifier already used with different content')
            return {**previous, '_existing': True}
        self.seq += 1
        row = {'seq': self.seq, 'id': id, 'thread': thread, 'role': role, 'text': text, 'name': name, 'session': session,
               'revision': revision, 'time': int(at) if at else int(time.time() * 1000), 'status': status,
               'language': language, 'offline': offline,
               'payload': json.dumps(payload) if payload else None, 'audio_reason': None, 'attempts': 0, 'next_attempt': 0,
               # The room's own clock, for the outbox: `time` is the speaker's and may be anything.
               'queued_at': int(time.time())}
        self.messages[id] = row
        while len(self.messages) > self.MAX_MESSAGES:
            oldest = next(iter(self.messages))
            if self.messages[oldest]['status'] in {'pending', 'sending'}:
                break   # never drop input that has not reached its harness
            del self.messages[oldest]
        return dict(row)

    def get(self, id):
        row = self.messages.get(id)
        return dict(row) if row else None

    def find_message(self, message_id):
        """The user row that carried this message id to the harness, or None."""
        if not isinstance(message_id, str) or not message_id:
            return None
        for row in reversed(self.messages.values()):
            if row['role'] == 'user' and row['payload'] and ('"message_id": "' + message_id + '"') in row['payload']:
                return dict(row)
        return None

    def update(self, id, status, reason=None):
        row = self.messages.get(id)
        if row:
            row['status'], row['audio_reason'] = status, reason

    def defer(self, id, *, immediate=False):
        """A delivery attempt failed or was lost: schedule the next one. At-least-once while the room runs."""
        row = self.messages.get(id)
        if not row:
            return
        row['attempts'] = (row['attempts'] or 0) + 1
        delay = 0 if immediate else RETRY_BACKOFF[min(row['attempts'], len(RETRY_BACKOFF)) - 1]
        row['status'], row['next_attempt'] = 'pending', int(time.time()) + delay

    def recover(self):
        # The journal starts empty with the process; a delivery in flight when it died is gone with it.
        for row in self.messages.values():
            if row['role'] == 'user' and row['status'] == 'sending':
                row['status'] = 'pending'
            elif row['role'] == 'assistant' and row['status'] in {'queued', 'synthesizing', 'playing', 'waiting_for_turn', 'waiting_for_pause'}:
                row['status'], row['audio_reason'] = 'interrupted', 'service_restarted'

    def expire_pending(self, now=None):
        """Input the room has held for longer than it is worth holding. Returns the rows it gave up on,
        so whoever asked can tell the browser that said them: nothing is dropped in silence."""
        now = int(now if now is not None else time.time())
        expired = []
        for row in self.messages.values():
            if row['role'] != 'user' or row['status'] != 'pending':
                continue
            if now - int(row.get('queued_at') or 0) < PENDING_TTL:
                continue
            row['status'], row['audio_reason'] = 'not_sent', 'expired'
            expired.append(dict(row))
        return expired

    def pending(self, now=None):
        now = int(now if now is not None else time.time())
        rows = [row for row in self.messages.values()
                if row['role'] == 'user' and row['status'] == 'pending' and (row['next_attempt'] or 0) <= now]
        return [dict(row) for row in rows[:32]]

    def history(self, thread=None):
        rows = [row for row in self.messages.values() if thread is None or row['thread'] == thread]
        return [{key: row[key] for key in HISTORY_KEYS} for row in rows[-1000:]]

    # ----- connectors: pairing and credentials (durable) -----

    # Crockford's base32: 32 symbols, no I, L, O or U, so a code survives being read aloud, dictated to an
    # agent or typed from a phone. Twelve symbols are 60 bits — against a ten-minute window and the
    # redemption limit in connector_control, not a budget anyone can spend — shown as three groups of four.
    # Three minutes of life: the operator cut it from ten (2026-09-22), since the code is used the moment it is shown.
    PAIRING_ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
    PAIRING_LENGTH = 12

    @classmethod
    def normalise_pairing_code(cls, code):
        """What a person typed or said, as the code it names: case, separators and the look-alikes Crockford
        decodes (O as 0, I and L as 1) are forgiven. Anything else is simply not a code."""
        text = ''.join(ch for ch in str(code or '').upper() if ch not in ' -_.')
        return text.translate(str.maketrans('OIL', '011'))

    PAIRING_TTL = 180   # a code is read off the screen and used at once; three minutes is generous

    def create_pairing_code(self, ttl=PAIRING_TTL):
        now = int(time.time())
        self.pairing_codes = {code: entry for code, entry in self.pairing_codes.items() if entry['expires'] >= now}
        code = ''.join(secrets.choice(self.PAIRING_ALPHABET) for _ in range(self.PAIRING_LENGTH))
        self.pairing_codes[code] = {'expires': now + ttl, 'redeemed': False}
        return '-'.join(code[i:i + 4] for i in range(0, self.PAIRING_LENGTH, 4))

    def redeem_pairing_code(self, code, identity=None):
        """One-time exchange: a valid code becomes a connector credential. Returns (id, token) or None."""
        entry = self.pairing_codes.get(self.normalise_pairing_code(code))
        if not entry or entry['redeemed'] or entry['expires'] < int(time.time()):
            return None
        entry['redeemed'] = True
        connector_id, token = str(uuid.uuid4()), secrets.token_urlsafe(32)
        now = int(time.time())
        self.connectors[connector_id] = {'token_hash': _hash(token), 'created': now, 'last_seen': now, 'revoked': 0,
                                         **connector_identity(identity)}
        self._save_state()
        return connector_id, token

    def connector_credential(self, connector_id, token):
        """What this credential is worth: 'paired', 'revoked' — this machine's own credential, taken away
        from the room — or 'unknown'. A wrong token is never told which of the two it is: the id alone
        says nothing about a machine somebody else paired."""
        if not isinstance(connector_id, str) or not isinstance(token, str) or not token:
            return 'unknown'
        entry = self.connectors.get(connector_id)
        if not entry or not secrets.compare_digest(entry['token_hash'], _hash(token)):
            return 'unknown'
        return 'revoked' if entry.get('revoked') else 'paired'

    def authenticate_connector(self, connector_id, token, identity=None):
        """A credential that still stands, and — since a machine says who it is on every connection —
        the latest it told us about itself, kept where the pairing is kept."""
        if self.connector_credential(connector_id, token) != 'paired':
            return False
        entry = self.connectors[connector_id]
        entry.update(last_seen=int(time.time()), **connector_identity(identity))
        self._save_state()
        return True

    def revoke_connector(self, connector_id):
        entry = self.connectors.get(connector_id)
        if entry:
            entry['revoked'] = 1
            self._save_state()
        for binding in self._bindings.values():
            if binding['connector'] == connector_id:
                binding['active'] = 0

    def forget_connector(self, connector_id):
        """Take the row away. Only ever asked of a pairing already revoked: a machine that is merely
        gone from the list would pair itself back in with the credential it still has on disk."""
        if self.connectors.pop(connector_id, None) is None:
            return False
        self._save_state()
        return True

    def paired_connectors(self):
        """Every machine paired with this room, for the room's own page — each as it last described
        itself. (Not `connectors`: that name is the dict this method reads, and an instance attribute
        shadows a method — the endpoint 500ed for as long as both existed, 2026-09-21.)"""
        return [{'id': cid, 'created': e.get('created'), 'last_seen': e.get('last_seen'), 'revoked': e.get('revoked', 0),
                 **{key: e.get(key) for key in IDENTITY_KEYS}}
                for cid, e in sorted(self.connectors.items(), key=lambda item: item[1].get('created') or 0)]

    # ----- bindings: which connector serves which conversation (memory only) -----

    def register_binding(self, connector, *, harness, thread, title=None, binding_id=None, inbound=None, capabilities=None, engine=None):
        """Server-minted ids. Reusing another connector's binding is refused; an id this room no longer
        knows (it restarted) is simply a fresh registration, so a façade never stays joined to nothing."""
        if not isinstance(thread, str) or not thread or len(thread) > 200:
            raise ValueError('A conversation identifier is required')
        row = self._bindings.get(binding_id) if binding_id else None
        if row and row['connector'] != connector:
            raise ValueError('Unknown or foreign binding')
        if row is None:
            candidates = [b for b in self._bindings.values() if b['connector'] == connector and b['thread'] == thread and b['active']]
            row = max(candidates, key=lambda b: b['created']) if candidates else None
        if row:
            row['active'], row['harness'] = 1, harness
            if title:
                row['title'] = title
            if inbound is not None:
                row['inbound'] = json.dumps(inbound)
            if capabilities is not None:
                row['capabilities'] = dict(capabilities)
            if engine is not None:
                row['engine'] = dict(engine)
            return dict(row)
        new = {'id': str(uuid.uuid4()), 'connector': connector, 'harness': harness, 'thread': thread, 'title': title,
               'created': int(time.time()), 'active': 1, 'inbound': json.dumps(inbound) if inbound is not None else None,
               'capabilities': dict(capabilities) if capabilities is not None else None,
               'engine': dict(engine) if engine is not None else None}
        self._bindings[new['id']] = new
        return dict(new)

    def set_binding_engine(self, binding_id, engine):
        """What this conversation thinks with, as its connector observed it. The launch-line value is
        what the binding carries until then; a binding this room no longer knows is left alone."""
        row = self._bindings.get(binding_id)
        if row is None:
            return None
        row['engine'] = dict(engine)
        return dict(row)

    def binding(self, binding_id):
        row = self._bindings.get(binding_id)
        return dict(row) if row else None

    def binding_for_thread(self, thread):
        candidates = [b for b in self._bindings.values() if b['thread'] == thread and b['active']]
        return dict(max(candidates, key=lambda b: b['created'])) if candidates else None

    def bindings(self):
        return [dict(b) for b in sorted(self._bindings.values(), key=lambda b: b['created']) if b['active']]

    def deactivate_binding(self, connector, binding_id):
        row = self._bindings.get(binding_id)
        if row and row['connector'] == connector:
            row['active'] = 0
