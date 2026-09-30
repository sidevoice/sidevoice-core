import asyncio
import tempfile
import time
import unittest
from pathlib import Path
from sidevoice.room_history import RoomHistory
from sidevoice.connector_control import ConnectorControl, ConnectorPeer, PROTOCOL


class FakePeer(ConnectorPeer):
    """A connector at the other end of nothing: what the room said to it, and what it answers.

    `answers` are handed back to the deliveries in order — an exception is raised instead, which
    is how the room learns one went nowhere. Asked for a delivery with no answer queued, it stays
    silent for ever, which is exactly what a harness that is thinking looks like: the delivery
    hangs in flight until the test settles it or takes it away.
    """

    def __init__(self, answers=()):
        self.sent, self.asked, self.answers, self.disconnected = [], [], list(answers), False

    async def send(self, event, data):
        self.sent.append((event, data))

    async def request(self, event, data, *, timeout):
        self.asked.append((event, data))
        if not self.answers:
            await asyncio.Event().wait()
        answer = self.answers.pop(0)
        if isinstance(answer, BaseException):
            raise answer
        return answer

    async def disconnect(self):
        self.disconnected = True

    def deliveries(self, field):
        return [data[field] for event, data in self.asked if event == 'input.deliver']

    async def until_asked(self, count):
        """Wait until the room has actually put that many questions to this connector. A tick
        chooses what to deliver and hands the asking to the task that will wait for the answer;
        the pump's own interval is what gives that task room to run."""
        for _ in range(100):
            if len(self.asked) >= count:
                return
            await asyncio.sleep(0)
        raise AssertionError(f'only {len(self.asked)} of {count} deliveries were ever asked')


class FakeHub:
    def __init__(self, journal):
        self.journal = journal; self.activated = []; self.published = []; self.receipts = []; self.working = []; self.cleared = []
    async def activate(self, target): self.activated.append(target)
    def conversation_working(self, thread, working, **correlation): self.working.append((thread, working, correlation))
    def clear_conversation_working(self, thread): self.cleared.append(thread)
    async def publish(self, speech): self.published.append(speech); return {'status': 'queued', 'text_saved': True, 'utterance_id': speech.utterance_id}
    def delivery_status(self, row_id, status): self.receipts.append((row_id, status))


class ControlPlaneTests(unittest.IsolatedAsyncioTestCase):
    """The control plane on its own: what each event means, with a peer that carries nothing.
    What carries them is `test_connector_socketio`."""

    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.journal = RoomHistory(Path(self.temp.name) / 'room.sqlite3')
        self.hub = FakeHub(self.journal)
        self.control = ConnectorControl(self.journal, self.hub, heartbeat_seconds=0.05)
        self.connector_id, self.token = self.journal.redeem_pairing_code(self.journal.create_pairing_code())

    async def asyncTearDown(self):
        for binding_id in list(self.control.inflight):
            self.control.drop_inflight(binding_id)
        self.temp.cleanup()

    def queue_input(self, thread, text, message_id='m1'):
        return self.journal.put(id='call:user-turn:' + message_id, thread=thread, role='user', text=text, name='Tú', session='call', revision=1,
                                status='pending', payload={'thread_id': thread, 'text': text, 'message_id': message_id, 'session_id': 'call', 'revision': 1})

    async def attach(self, answers=()):
        peer = FakePeer(answers)
        await self.control.attach(self.connector_id, peer)
        return peer

    async def join(self, thread, **extra):
        """A conversation registered by a connector that is connected, as the wire would do it."""
        if self.connector_id not in self.control.peers:
            await self.attach()
        return await self.control.register(self.connector_id, {'client_ref': thread, 'harness': 'claude',
                                                               'thread': thread, 'title': 'Trabajo', **extra})

    async def test_a_binding_carries_which_model_answers_it(self):
        # Read by the harness from its own launch line, never asked of the model itself.
        await self.control.register(self.connector_id, {'client_ref': 'thread-a',
            'thread': 'thread-a', 'harness': 'claude', 'title': 'A',
            'engine': {'model': 'claude-opus-5', 'effort': 'high', 'thinking': 'adaptive', 'junk': 'x'}})
        binding = self.control.participants()[0]
        self.assertEqual(binding['engine'], {'model': 'claude-opus-5', 'effort': 'high', 'thinking': 'adaptive'},
                         'only the three fields, and nothing invented')
        # A harness that cannot tell says nothing, and nothing is stored.
        await self.control.register(self.connector_id, {'client_ref': 'thread-b',
            'thread': 'thread-b', 'harness': 'codex', 'title': 'B', 'engine': 'gpt'})
        self.assertIsNone(self.control.participants()[1]['engine'])

    async def test_the_harness_observing_which_model_answers_replaces_what_the_launch_line_said(self):
        # A session launched with no --model says what it thinks with the moment its harness records it.
        registered = await self.join('sess-1', engine={'model': 'claude-opus-5'})
        await self.control.engine(self.connector_id, {'binding_id': registered['binding_id'],
                                                      'engine': {'model': 'claude-fable-5-1', 'effort': None, 'thinking': None}})
        self.assertEqual(self.journal.binding(registered['binding_id'])['engine'], {'model': 'claude-fable-5-1'},
                         'what was observed replaces the launch line, and null is not a value')
        # Nothing to name, nothing to store; and a binding this connector does not hold is not its business.
        await self.control.engine(self.connector_id, {'binding_id': registered['binding_id'], 'engine': {'effort': 'high'}})
        await self.control.engine(self.connector_id, {'binding_id': 'someone-elses', 'engine': {'model': 'gpt-5.6-terra'}})
        self.assertEqual(self.journal.binding(registered['binding_id'])['engine'], {'model': 'claude-fable-5-1'})

    async def test_revoking_a_machine_stops_it_serving_now_and_tells_it_why(self):
        # Taking a pairing away is not a note for the machine's next connection: the conversations it
        # carried lose their voice at once, the way they do when the room closes a channel.
        peer = await self.attach()
        first, second = await self.join('thread-a'), await self.join('thread-b')

        lost = await self.control.revoke(self.connector_id)

        self.assertEqual(sorted(record['thread'] for record in lost), ['thread-a', 'thread-b'])
        self.assertFalse(self.control.is_live(first['binding_id']))
        self.assertFalse(self.control.is_live(second['binding_id']))
        self.assertEqual(self.journal.bindings(), [], 'nothing of that machine is still bound')
        self.assertEqual(self.control.peers, {}, 'and nothing is still reachable through it')
        self.assertTrue(peer.disconnected)
        said = dict((event, data) for event, data in peer.sent)
        self.assertEqual([data['reason'] for event, data in peer.sent if event == 'binding.close'],
                         ['connector_revoked', 'connector_revoked'])
        self.assertIn('Emparejar máquina', said['connector.revoked']['reason'],
                      'a connector told why stops asking, instead of reading a closed socket')
        self.assertEqual(self.journal.connector_credential(self.connector_id, self.token), 'revoked')

    async def test_a_machine_says_who_it_is_at_pairing_and_again_on_every_connection(self):
        journal = RoomHistory(Path(self.temp.name) / 'identity.json')
        connector_id, token = journal.redeem_pairing_code(
            journal.create_pairing_code(),
            {'host': 'macbook-pro', 'platform': 'darwin arm64', 'version': '0.4.3', 'harnesses': ['claude']})
        paired, = journal.paired_connectors()
        self.assertEqual((paired['host'], paired['platform'], paired['version'], paired['harnesses']),
                         ('macbook-pro', 'darwin arm64', '0.4.3', ['claude']))

        # The machine upgraded and grew a harness since: the row says what is true now.
        self.assertTrue(journal.authenticate_connector(connector_id, token,
                                                       {'version': '0.5.0', 'harnesses': ['claude', 'codex']}))
        upgraded, = journal.paired_connectors()
        self.assertEqual((upgraded['version'], upgraded['harnesses']), ('0.5.0', ['claude', 'codex']))
        self.assertEqual(upgraded['host'], 'macbook-pro', 'what it did not say again is not lost')

        # Nothing is invented: a connector that describes nothing leaves the row as it was.
        self.assertTrue(journal.authenticate_connector(connector_id, token, {'host': '', 'platform': None}))
        silent, = journal.paired_connectors()
        self.assertEqual((silent['host'], silent['platform']), ('macbook-pro', 'darwin arm64'))

    async def test_pairing_is_one_time_and_credentials_are_checked(self):
        code = self.journal.create_pairing_code()
        first = self.journal.redeem_pairing_code(code)
        self.assertIsNotNone(first)
        self.assertIsNone(self.journal.redeem_pairing_code(code))
        self.assertIsNone(self.journal.redeem_pairing_code('NOPE'))
        self.assertTrue(self.journal.authenticate_connector(*first))
        self.assertFalse(self.journal.authenticate_connector(first[0], 'wrong'))
        self.journal.revoke_connector(first[0])
        self.assertFalse(self.journal.authenticate_connector(*first))

    async def test_pairings_survive_a_restart_but_the_journal_does_not(self):
        from sidevoice.room_history import RoomHistory
        self.queue_input('thread-x', 'Said before the restart')
        restarted = RoomHistory(self.journal.path)
        self.assertTrue(restarted.authenticate_connector(self.connector_id, self.token))
        self.assertEqual(restarted.history(), [])
        self.assertEqual(restarted.bindings(), [])
        self.assertEqual(oct(restarted.state_path.stat().st_mode)[-3:], '600')

    async def test_a_legacy_database_is_imported_once_for_its_pairings(self):
        import sqlite3
        from sidevoice.room_history import RoomHistory
        root = Path(self.temp.name) / 'legacy'
        root.mkdir()
        db = sqlite3.connect(root / 'room-history.sqlite3')
        db.execute('CREATE TABLE connectors (id TEXT PRIMARY KEY, token_hash TEXT NOT NULL, host TEXT, created INTEGER, last_seen INTEGER, revoked INTEGER DEFAULT 0)')
        db.execute('CREATE TABLE closed_channels (thread TEXT PRIMARY KEY, notification TEXT)')
        db.execute('CREATE TABLE messages (id TEXT, text TEXT)')
        db.execute("INSERT INTO connectors VALUES ('old-connector', ?, 'laptop', 1, 2, 0)", (__import__('hashlib').sha256(b'old-token').hexdigest(),))
        db.execute("INSERT INTO messages VALUES ('m', 'a transcript that must stay where it is')")
        db.commit(); db.close()
        journal = RoomHistory(root / 'room-state.json')
        self.assertTrue(journal.authenticate_connector('old-connector', 'old-token'))
        self.assertEqual(journal.history(), [])
        self.assertTrue((root / 'room-state.json').exists())
        self.assertTrue((root / 'room-history.sqlite3').exists(), 'the old database is left for the operator to delete')

    async def test_closing_from_the_room_tells_the_connector_and_forgets_the_binding(self):
        peer = await self.attach()
        registered = await self.join('sess-1')
        await self.control.close_binding(self.journal.binding(registered['binding_id']))
        self.assertEqual(peer.sent, [('binding.close', {'binding_id': registered['binding_id'], 'thread': 'sess-1',
                                                        'reason': 'closed_from_room'})])
        self.assertFalse(self.control.is_live(registered['binding_id']))
        self.assertIsNone(self.journal.binding_for_thread('sess-1'))
        # Speech from a binding the room closed is refused, so nothing is stored for it.
        late = await self.control.speech(self.connector_id, {'event_id': 'sp', 'binding_id': registered['binding_id'],
                                                             'session_id': 's', 'revision': 1, 'text': 'tarde'})
        self.assertEqual(late['status'], 'rejected')
        self.assertEqual(self.hub.published, [])

    async def test_a_read_receipt_marks_the_row_read_only_for_its_own_binding_and_thread(self):
        registered = await self.join('sess-1')
        row = self.queue_input('sess-1', 'hola', message_id='m-read')
        self.journal.update(row['id'], 'unconfirmed')
        other = self.queue_input('sess-2', 'ajeno', message_id='m-other')
        for message_id in ('m-other', 'm-read', 'm-read'):
            await self.control.read(self.connector_id, {'binding_id': registered['binding_id'], 'message_id': message_id,
                                                        'session_id': 'call', 'revision': 1})
        self.assertEqual(self.journal.get(row['id'])['status'], 'read')
        self.assertEqual(self.journal.get(other['id'])['status'], 'pending')
        self.assertEqual(self.hub.receipts.count((row['id'], 'read')), 1)

    async def test_a_read_receipt_that_arrives_before_the_delivery_acknowledgement_is_not_downgraded(self):
        peer = await self.attach()
        registered = await self.join('sess-1')
        row = self.queue_input('sess-1', 'hola', message_id='m-fast')
        await self.control.tick(); await peer.until_asked(1)   # in flight, waiting for the harness's answer
        self.assertEqual(peer.deliveries('message_id'), ['m-fast'])
        self.assertEqual(self.journal.get(row['id'])['status'], 'sending')
        await self.control.read(self.connector_id, {'binding_id': registered['binding_id'], 'message_id': 'm-fast', 'session_id': 'call', 'revision': 1})
        self.assertEqual(self.journal.get(row['id'])['status'], 'read')
        await self.control.acknowledge(self.connector_id, row['id'], {'status': 'unknown', 'detail': 'inbox'})
        self.assertEqual(self.journal.get(row['id'])['status'], 'read', 'a late acknowledgement never takes the second tick away')
        self.assertEqual([status for (identifier, status) in self.hub.receipts if identifier == row['id']][-1], 'read')

    async def test_the_harness_saying_it_is_working_reaches_the_browsers_on_that_conversation(self):
        registered = await self.join('sess-1')
        for said in ({'working': True, 'turn_id': 'turn-1', 'turn_phase': 'start', 'session_id': 'call', 'revision': 4},
                     {'working': False, 'turn_id': 'turn-1', 'turn_phase': 'end', 'session_id': 'call', 'revision': 4},
                     {}):
            await self.control.working(self.connector_id, {'binding_id': registered['binding_id'], **said})
        # A binding this connector does not hold says nothing about anyone.
        await self.control.working(self.connector_id, {'binding_id': 'someone-elses', 'working': True})
        self.assertEqual(self.hub.working, [
            ('sess-1', True, {'turn_id': 'turn-1', 'turn_phase': 'start', 'session_id': 'call', 'revision': 4}),
            ('sess-1', False, {'turn_id': 'turn-1', 'turn_phase': 'end', 'session_id': 'call', 'revision': 4})])

    async def test_binding_keeps_declared_capabilities_and_missing_ones_are_unknown(self):
        await self.join('thread-1', harness='codex',
                        capabilities={'deliver': 'supported', 'working': 'unsupported', 'inspectInbound': False})
        participant = self.control.participants()[0]
        self.assertEqual(participant['capabilities'], {
            'deliver': 'supported',
            'inspectInbound': 'unknown',
            'working': 'unsupported',
            'endOfTurn': 'unknown',
            'sessionIdentity': 'unknown',
        })
        self.assertIsNone(self.journal.binding(participant['id']).get('inbound'))

    async def test_an_unknown_binding_id_from_its_connector_is_a_fresh_registration(self):
        binding = self.journal.register_binding(self.connector_id, harness='claude', thread='sess-1', binding_id='gone-after-restart')
        self.assertNotEqual(binding['id'], 'gone-after-restart')
        again = self.journal.register_binding(self.connector_id, harness='claude', thread='sess-1', binding_id=binding['id'])
        self.assertEqual(again['id'], binding['id'])

    async def test_register_mints_id_focuses_nobody_and_an_unknown_id_is_a_fresh_registration(self):
        await self.attach()
        registered = await self.join('sess-1')
        self.assertEqual((registered['client_ref'], registered['thread']), ('sess-1', 'sess-1'))
        self.assertTrue(self.control.is_live(registered['binding_id']))
        self.assertEqual(self.hub.activated, [], 'a conversation joining selects itself for no browser')
        # An id the room does not know (it restarted) is not foreign: the same connector gets its binding back.
        reused = await self.control.register(self.connector_id, {'client_ref': 'r2', 'harness': 'claude',
                                                                 'thread': 'sess-1', 'binding_id': 'not-mine'})
        self.assertEqual(reused['binding_id'], registered['binding_id'])
        with self.assertRaises(ValueError):
            await self.control.register(self.connector_id, {'client_ref': 'r3', 'harness': 'claude', 'thread': 'bad thread!'})
        self.assertEqual(self.journal.binding_for_thread('sess-1')['id'], registered['binding_id'])

    async def test_delivery_is_one_at_a_time_acknowledged_by_owner_and_retried_on_failure(self):
        binding = self.journal.register_binding(self.connector_id, harness='claude', thread='sess-1')
        peer = await self.attach()
        self.control.live[binding['id']] = self.connector_id
        first = self.queue_input('sess-1', 'primero', 'm1'); self.queue_input('sess-1', 'segundo', 'm2')
        await self.control.tick(); await peer.until_asked(1)
        self.assertEqual(peer.deliveries('text'), ['primero'])
        self.assertEqual(peer.deliveries('message_id'), ['m1'])
        await self.control.tick()
        self.assertEqual(len(peer.asked), 1, 'one at a time keeps the user\'s turns in order')
        await self.control.acknowledge('someone-else', first['id'], {'status': 'accepted'})
        self.assertEqual(self.journal.get(first['id'])['status'], 'sending')
        await self.control.acknowledge(self.connector_id, first['id'], {'status': 'failed'})
        row = self.journal.get(first['id'])
        self.assertEqual((row['status'], row['attempts']), ('pending', 1))
        self.assertGreater(row['next_attempt'], int(time.time()))  # backed off, not hammered
        await self.control.tick(now=time.time() + 100); await peer.until_asked(2)
        self.assertEqual(peer.deliveries('text'), ['primero', 'primero'])
        await self.control.acknowledge(self.connector_id, first['id'], {'status': 'accepted'})
        self.assertEqual(self.journal.get(first['id'])['status'], 'delivered')
        self.assertIn((first['id'], 'delivered'), self.hub.receipts)
        await self.control.tick(now=time.time() + 100); await peer.until_asked(3)
        self.assertEqual(peer.deliveries('text')[-1], 'segundo')

    async def test_an_unanswered_delivery_is_backed_off_and_an_unasked_one_goes_straight_back(self):
        # The two ways a delivery fails are not the same: a harness that is thinking has to be
        # given room, while a connection that died must not make the next one wait for it.
        binding = self.journal.register_binding(self.connector_id, harness='claude', thread='sess-1')
        peer = await self.attach([TimeoutError('input.deliver went unacknowledged for 1s')])
        self.control.live[binding['id']] = self.connector_id
        row = self.queue_input('sess-1', 'hola')
        await self.control.tick()
        await self.settled()
        timed_out = self.journal.get(row['id'])
        self.assertEqual((timed_out['status'], timed_out['attempts']), ('pending', 1))
        self.assertGreater(timed_out['next_attempt'], int(time.time()), 'a harness that is thinking is given room')
        self.assertEqual(self.control.inflight, {})

        peer.answers.append(RuntimeError('the socket went'))
        await self.control.tick(now=time.time() + 100)
        await self.settled()
        dropped = self.journal.get(row['id'])
        self.assertEqual(dropped['status'], 'pending')
        self.assertLessEqual(dropped['next_attempt'], int(time.time()), 'what was never asked waits for nothing')
        self.assertEqual(self.control.inflight, {})

    async def test_a_connection_that_goes_puts_what_it_held_back_in_the_outbox(self):
        binding = self.journal.register_binding(self.connector_id, harness='claude', thread='sess-1')
        peer = await self.attach()
        self.control.live[binding['id']] = self.connector_id
        row = self.queue_input('sess-1', 'hola')
        await self.control.tick()
        self.assertEqual(self.journal.get(row['id'])['status'], 'sending')
        self.control.detach(self.connector_id, peer)
        self.assertEqual(self.journal.get(row['id'])['status'], 'pending')
        self.assertEqual(self.control.inflight, {})
        self.assertFalse(self.control.is_live(binding['id']))

    async def test_a_conversation_nobody_can_reach_is_not_working(self):
        """A connector that goes leaves the room holding the last thing it said about every conversation
        it carried. Told "working", the room would show it thinking for ever, next to a dot that says it
        is not there."""
        binding = self.journal.register_binding(self.connector_id, harness='claude', thread='sess-1')
        peer = await self.attach()
        self.control.live[binding['id']] = self.connector_id
        await self.control.working(self.connector_id, {'binding_id': binding['id'], 'working': True})
        self.assertIn(('sess-1', True, {}), self.hub.working)
        self.control.detach(self.connector_id, peer)
        self.assertIn('sess-1', self.hub.cleared, 'the work is put out with the connection')

    async def test_input_the_room_held_too_long_is_never_delivered(self):
        """A machine that drops and comes back loses nothing: that is what the outbox is for. But an hour
        later the same sentence is not something to answer, and the room says it gave up rather than
        handing it over as if it had just been said."""
        from sidevoice import room_history
        binding = self.journal.register_binding(self.connector_id, harness='claude', thread='sess-1')
        peer = await self.attach()
        self.control.live[binding['id']] = self.connector_id
        row = self.queue_input('sess-1', 'lo dije cuando no estabas')
        self.journal.get(row['id'])
        # Said one second inside the window, and then one second outside it.
        now = time.time() + room_history.PENDING_TTL - 1
        self.assertEqual(self.journal.expire_pending(now), [], 'nothing is given up on while it is worth holding')
        self.assertEqual(self.journal.get(row['id'])['status'], 'pending')
        now += 2
        # The tick gives up on it before it chooses anything to send, and the browser that said it hears
        # about it: nothing is dropped in silence.
        await self.control.tick(now)
        self.assertEqual(self.journal.get(row['id'])['status'], 'not_sent')
        self.assertEqual(self.journal.get(row['id'])['audio_reason'], 'expired')
        self.assertIn((row['id'], 'not_sent'), self.hub.receipts)
        self.assertEqual(self.journal.pending(now), [], 'and it is never chosen for delivery again')
        self.assertEqual(self.control.inflight, {}, 'nor sent by the very tick that gave up on it')
        self.control.detach(self.connector_id, peer)

    async def test_speech_lands_in_room_only_from_owning_connector(self):
        binding = self.journal.register_binding(self.connector_id, harness='claude', thread='sess-1')
        self.control.live[binding['id']] = self.connector_id
        published = await self.control.speech(self.connector_id, {'event_id': 'e1', 'binding_id': binding['id'], 'session_id': 'call', 'revision': 3, 'text': 'Hola', 'language': 'es'})
        self.assertEqual(published['status'], 'queued'); self.assertEqual(published['event_id'], 'e1')
        self.assertEqual((self.hub.published[0].thread_id, self.hub.published[0].revision), ('sess-1', 3))
        refused = await self.control.speech('intruder', {'event_id': 'e2', 'binding_id': binding['id'], 'session_id': 'call', 'revision': 3, 'text': 'Hola'})
        self.assertEqual(refused['status'], 'rejected'); self.assertEqual(len(self.hub.published), 1)

    async def test_newer_connection_from_same_connector_wins(self):
        first = await self.attach()
        second = await self.attach()
        self.assertIs(self.control.peers[self.connector_id], second)
        self.assertTrue(first.disconnected)
        self.assertFalse(second.disconnected)
        # The loser's own teardown arrives afterwards and takes nothing from the winner.
        self.control.detach(self.connector_id, first)
        self.assertIs(self.control.peers[self.connector_id], second)

    async def settled(self):
        """Let the tasks that are waiting for acknowledgements reach their conclusion."""
        for _ in range(50):
            await asyncio.sleep(0)
            if not any(entry[2] and not entry[2].done() for entry in self.control.inflight.values()):
                return
        raise AssertionError('a delivery never settled')


class PairingCodeSurfaceTests(unittest.IsolatedAsyncioTestCase):
    """The code is shown to the person in the room, never handed to whoever can reach the address."""

    async def test_a_pairing_code_is_only_given_to_the_room_page(self):
        from fastapi import FastAPI
        from starlette.testclient import TestClient
        from sidevoice.connector_control import mount_connector_control
        app = FastAPI()
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        mount_connector_control(app, FakeHub(RoomHistory(Path(temp.name) / 'room.sqlite3')), heartbeat_seconds=5)
        with TestClient(app) as client:
            from_a_client = client.post('/api/connectors/pairing-code')
            self.assertEqual(from_a_client.status_code, 403, 'a command line reaching the address is not someone in the room')
            from_elsewhere = client.post('/api/connectors/pairing-code', headers={'Origin': 'http://evil.example'})
            self.assertEqual(from_elsewhere.status_code, 403)
            from_the_room = client.post('/api/connectors/pairing-code', headers={'Origin': 'http://testserver'})
            self.assertEqual(from_the_room.status_code, 200)
            # Sixty bits in an alphabet that survives being read aloud: no I, L, O or U, three groups of four.
            self.assertRegex(from_the_room.json()['code'], r'^[0-9A-HJKMNP-TV-Z]{4}-[0-9A-HJKMNP-TV-Z]{4}-[0-9A-HJKMNP-TV-Z]{4}$')
            # Redeeming needs no browser: that step is the machine's, with the code the person carried to it.
            # Redeemed as a person would type or dictate it: lower case, no dashes, a look-alike letter.
            spoken = from_the_room.json()['code'].replace('-', ' ').lower().replace('0', 'o', 1)
            redeemed = client.post('/api/connectors/pair', json={'code': spoken, 'host': 'laptop'})
            self.assertEqual(redeemed.status_code, 200)
            self.assertIn('token', redeemed.json())
            self.assertEqual(redeemed.json()['protocol'], PROTOCOL)
            # The room's own page can list what is paired, and see that this one is not connected yet.
            listed = client.get('/api/connectors', headers={'Origin': 'http://testserver'})
            self.assertEqual(listed.status_code, 200, listed.text)
            self.assertEqual([(c['host'], c['connected']) for c in listed.json()['connectors']], [('laptop', False)])

    async def test_guessing_codes_locks_redemption_for_the_whole_room(self):
        from fastapi import FastAPI
        from starlette.testclient import TestClient
        from sidevoice.connector_control import mount_connector_control
        app = FastAPI()
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        journal = RoomHistory(Path(temp.name) / 'room.sqlite3')
        mount_connector_control(app, FakeHub(journal), heartbeat_seconds=5, redemption_limit={'failures': 3, 'window': 600})
        with TestClient(app) as client:
            real = client.post('/api/connectors/pairing-code', headers={'Origin': 'http://testserver'}).json()['code']
            for _ in range(3):
                self.assertEqual(client.post('/api/connectors/pair', json={'code': 'NOPE-NOPE-NOPE'}).status_code, 403)
            # The fourth wrong one, and even the right one, are refused for the window: a guesser learns nothing.
            locked = client.post('/api/connectors/pair', json={'code': 'NOPE-NOPE-NOPE'})
            self.assertEqual(locked.status_code, 429)
            self.assertTrue(int(locked.headers['Retry-After']) > 0)
            self.assertEqual(client.post('/api/connectors/pair', json={'code': real}).status_code, 429)
            self.assertIsNotNone(journal.pairing_codes.get(RoomHistory.normalise_pairing_code(real)), 'the real code was not spent by the lockout')

class RevocationSurfaceTests(unittest.IsolatedAsyncioTestCase):
    """Taking a machine's pairing away: the person in the room does it, and nobody else can."""

    def room(self):
        from fastapi import FastAPI
        from starlette.testclient import TestClient
        from sidevoice.connector_control import mount_connector_control
        app = FastAPI()
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        journal = RoomHistory(Path(temp.name) / 'room-state.json')
        control = mount_connector_control(app, FakeHub(journal), heartbeat_seconds=5)
        return TestClient(app), journal, control

    def machine(self, client, **identity):
        code = client.post('/api/connectors/pairing-code', headers={'Origin': 'http://testserver'}).json()['code']
        paired = client.post('/api/connectors/pair', json={'code': code, **identity})
        self.assertEqual(paired.status_code, 200, paired.text)
        return paired.json()['connector_id']

    async def test_only_the_room_page_revokes_and_a_second_time_takes_the_row_away(self):
        client, journal, control = self.room()
        with client:
            connector_id = self.machine(client, host='laptop')
            peer = FakePeer()
            await control.attach(connector_id, peer)

            self.assertEqual(client.delete(f'/api/connectors/{connector_id}').status_code, 403,
                             'reaching the address is not being in the room')
            self.assertEqual(client.delete(f'/api/connectors/{connector_id}',
                                           headers={'Origin': 'http://evil.example'}).status_code, 403)

            revoked = client.delete(f'/api/connectors/{connector_id}', headers={'Origin': 'http://testserver'})
            self.assertEqual(revoked.status_code, 200, revoked.text)
            self.assertEqual(revoked.json()['status'], 'revoked')
            self.assertTrue(peer.disconnected, 'the live machine stops serving now, not on its next connection')
            listed = client.get('/api/connectors', headers={'Origin': 'http://testserver'}).json()['connectors']
            self.assertEqual([(row['host'], row['revoked'], row['connected']) for row in listed], [('laptop', 1, False)],
                             'it stays listed as revoked, so nobody wonders why that machine went quiet')

            removed = client.delete(f'/api/connectors/{connector_id}', headers={'Origin': 'http://testserver'})
            self.assertEqual(removed.json()['status'], 'removed')
            self.assertEqual(client.get('/api/connectors', headers={'Origin': 'http://testserver'}).json()['connectors'], [])
            self.assertEqual(client.delete(f'/api/connectors/{connector_id}',
                                           headers={'Origin': 'http://testserver'}).status_code, 404)

    async def test_the_list_carries_what_each_machine_said_about_itself(self):
        client, journal, control = self.room()
        with client:
            self.machine(client, host='macbook-pro', platform='darwin arm64', version='0.4.3',
                         harnesses=['claude', 'codex'])
            row, = client.get('/api/connectors', headers={'Origin': 'http://testserver'}).json()['connectors']
            self.assertEqual((row['host'], row['platform'], row['version'], row['harnesses']),
                             ('macbook-pro', 'darwin arm64', '0.4.3', ['claude', 'codex']),
                             'a row reads as a machine, not as a UUID')
