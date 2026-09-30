"""Regressions for a room with more than one browser in it.

Everything here asks the same question in a different place: does what belongs to
one browser stay in that browser, and does what belongs to the room stay shared?
"""
import asyncio
import os
import tempfile
from pathlib import Path
from unittest import IsolatedAsyncioTestCase
from unittest.mock import AsyncMock, patch

from fastapi import HTTPException

from sidevoice.presentation import Speech
from sidevoice.room import Room, RoomClient
from sidevoice.room_history import RoomHistory
from sidevoice.synthesis_cache import SynthesisCache

KOKORO = {'provider': 'kokoro', 'model': 'kokoro', 'voice': 'ef_dora', 'speed': 1.0, 'language': 'es'}
ELEVEN = {'provider': 'elevenlabs', 'model': 'eleven_v3', 'voice': 'una-voz', 'speed': 1.0, 'language': 'es'}


class RoomFixture(IsolatedAsyncioTestCase):
    """One room, a stand-in for the paid engine, and browsers that record what they were handed."""

    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.renders = []
        self.hub = Room(RoomHistory(self.path('history.sqlite3')), SynthesisCache(renderer=self.render))
        self.voice = KOKORO
        voices = patch('sidevoice.language_settings.resolve_voice', side_effect=lambda *a, **k: dict(self.voice))
        voices.start()
        self.addCleanup(voices.stop)

    def path(self, name):
        return Path(self.temp.name) / name

    async def render(self, choice, text):
        """Stand in for ElevenLabs: one call here is one call the room would have paid for."""
        self.renders.append((choice['provider'], choice['voice'], text))
        return {'mime_type': 'audio/mpeg', 'audio_base64': 'YQ==',
                'timings_ms': {'request_to_complete_ms': 20},
                'alignment': {'characters': list(text[:3])}}

    def browser(self, session_id):
        client = RoomClient(session_id, self.hub, worker=AsyncMock())
        client.connected = True
        client.target = {'thread_id': 'task', 'title': 'Tarea', 'binding_id': 'bind-' + session_id}
        client.heard = []
        client.on_browser_event = client.heard.append
        return client

    def spoken(self, client):
        return [event['data'] for event in client.heard
                if event['type'] in {'voice-speech', 'voice-speech-audio'}]

    def row(self, utterance_id, session_id):
        return self.hub.journal.get(session_id + ':voice:' + utterance_id)

    async def reply(self, client, utterance_id, text='Ya está listo', **extra):
        return await self.hub.publish(Speech(thread_id='task', session_id=client.id,
                                             revision=client.revision, text=text,
                                             utterance_id=utterance_id, **extra))


class MultiClientRoomTests(RoomFixture):
    """Does what belongs to one browser stay in that browser, and what belongs to the room stay shared?"""

    # ----- the room is shared -----

    async def test_a_browser_selecting_mid_turn_receives_the_current_working_state(self):
        first, second = self.browser('one'), self.browser('two')
        second.target = {'thread_id': 'other', 'title': 'Other', 'binding_id': 'other-two'}
        self.hub.conversation_working('task', True, turn_id='turn-1', turn_phase='start')
        self.assertEqual(first.heard[-1]['data']['working'], True)
        self.assertEqual(second.heard, [])
        await self.hub.select(second.id, 'task', 'Tarea')
        self.assertEqual(second.heard[-1], {'type': 'voice-conversation',
                                            'data': {'thread_id': 'task', 'working': True}})

    async def test_two_browsers_stay_connected_and_both_hear_one_shared_reply(self):
        first, second = self.browser('one'), self.browser('two')
        result = await self.reply(first, 'shared')
        self.assertEqual(result['status'], 'synthesizing')
        self.assertEqual(len(self.hub.listeners()), 2)
        for client in (first, second):
            spoken = self.spoken(client)
            self.assertEqual(len(spoken), 1)
            self.assertEqual((spoken[0]['session_id'], spoken[0]['text'], spoken[0]['utterance_id']),
                             (client.id, 'Ya está listo', 'shared'))
            self.assertEqual(spoken[0]['history_id'], 'one:voice:shared')
        # One reply, one row, one text; two independent playbacks of it.
        self.assertEqual(len(self.hub.journal.history('task')), 1)
        self.assertEqual(set(self.hub.utterances['shared'].clients), {'one', 'two'})

    async def test_a_reply_answering_one_browser_still_reaches_the_other(self):
        first, second = self.browser('one'), self.browser('two')
        first.user_started(); first.speaking = False
        await self.reply(first, 'answer-to-one')
        self.assertEqual(len(self.spoken(second)), 1)
        self.assertEqual(self.spoken(second)[0]['session_id'], 'two')

    async def test_a_turn_in_one_browser_moves_only_that_browsers_epoch(self):
        first, second = self.browser('one'), self.browser('two')
        await self.reply(first, 'stale')
        second.user_started()
        self.assertEqual((first.revision, second.revision), (0, 1))
        # Only the browser whose user spoke drops the audio; the other keeps playing the shared reply.
        self.assertEqual(second.heard[-1]['type'], 'voice-cancel')
        self.assertEqual(second.heard[-1]['data']['revision'], 1)
        self.assertNotEqual(first.heard[-1]['type'], 'voice-cancel')
        self.assertEqual(self.hub.utterances['stale'].clients['one']['status'], 'synthesizing')
        self.assertEqual(self.hub.utterances['stale'].clients['two']['status'], 'waiting_for_turn')
        with self.assertRaises(HTTPException):
            await self.hub.speak('Respuesta de un turno viejo', 'late', second.id, 0)

    async def test_a_reply_held_behind_another_reply_is_not_waiting_for_anybodys_turn(self):
        client = self.browser('one')
        client.audio_grace_seconds = 0
        await self.reply(client, 'ahead')
        await self.reply(client, 'behind', text='Y otra cosa')
        client.user_started()
        self.assertEqual(self.hub.utterances['behind'].clients['one']['status'], 'waiting_for_turn')
        await client.finish_user_turn()
        self.assertEqual(client.active, 'ahead')
        # Nobody is talking any more: the wait is the reply ahead of it, and the transcript must say so.
        self.assertEqual(self.hub.utterances['behind'].clients['one'],
                         {'status': 'queued', 'reason': 'previous_reply'})
        self.assertEqual(self.row('behind', 'one')['audio_reason'], 'previous_reply')

    async def test_a_reply_whose_ending_never_comes_back_does_not_hold_the_queue_for_ever(self):
        # Seen live on 2026-09-20: a reply stayed "playing" with no receipt and two more waited behind it (#60).
        client = self.browser('one')
        client.audio_grace_seconds = 0
        # The bound grows with the text: a word expires fast, a long reply is given its time.
        client.PLAYBACK_BASE_SECONDS, client.PLAYBACK_CHARS_PER_SECOND = 0.05, 100
        await self.reply(client, 'stuck', text='Nada')
        await self.reply(client, 'next', text='La siguiente, bastante más larga, ' * 4)
        self.assertEqual(client.active, 'stuck')
        client.transition('stuck', 'playing')
        await asyncio.sleep(0.2)
        self.assertEqual(self.hub.utterances['stuck'].clients['one'], {'status': 'failed', 'reason': 'unconfirmed'})
        self.assertEqual(self.row('stuck', 'one')['audio_reason'], 'unconfirmed')
        self.assertEqual(client.active, 'next', 'the queue moved on')

    async def test_skipping_a_reply_plays_the_next_at_once_and_it_is_never_offered_again(self):
        client = self.browser('one')
        client.audio_grace_seconds = 0
        await self.reply(client, 'long', text='Una respuesta que no quiero oír')
        await self.reply(client, 'next', text='La siguiente')
        client.transition('long', 'playing')
        self.assertTrue(await client.skipped('long', client.revision))
        self.assertEqual(self.hub.utterances['long'].clients['one'], {'status': 'interrupted', 'reason': 'user_skipped'})
        self.assertEqual(client.active, 'next', 'no turn is needed for the queue to move on')
        self.assertEqual(self.hub.missed_replies(client, seconds=120), [], 'skipped is heard enough')

    async def test_listening_again_uses_only_the_audio_the_room_holds(self):
        self.voice = ELEVEN
        client = self.browser('one')
        client.audio_grace_seconds = 0
        await self.reply(client, 'said', text='Esto ya sonó')
        await client.playback_finished('said', client.revision)
        self.assertIn('one:voice:said', self.hub.replayable_rows(client))
        again = await self.hub.replay_one(client, 'one:voice:said')
        self.assertEqual(self.renders, [('elevenlabs', 'una-voz', 'Esto ya sonó')], 'nothing bought twice')
        self.assertEqual(client.active, again['utterance_id'])
        self.assertTrue(self.spoken(client)[-1].get('requested'), 'the page is told it was asked for, so it plays it again')
        # Once the room no longer has the audio, there is no button and no repetition.
        self.hub.assets.entries.clear()
        self.assertNotIn('one:voice:said', self.hub.replayable_rows(client))
        with self.assertRaises(HTTPException):
            await self.hub.replay_one(client, 'one:voice:said')
        # A reply rendered in the browser never reached the room: nothing to repeat from.
        self.voice = KOKORO
        self.assertEqual(self.hub.replayable_rows(client), set())

    async def test_a_reply_that_ends_in_time_is_not_touched_by_its_bound(self):
        client = self.browser('one')
        client.audio_grace_seconds = 0
        client.PLAYBACK_BASE_SECONDS, client.PLAYBACK_CHARS_PER_SECOND = 0.05, 1e9
        await self.reply(client, 'fine', text='Suena y termina')
        client.transition('fine', 'playing')
        await client.playback_finished('fine', client.revision)
        await asyncio.sleep(0.15)
        self.assertEqual(self.hub.utterances['fine'].clients['one']['status'], 'playback_finished')

    # ----- playback is not -----

    async def test_one_browser_stopping_playback_leaves_the_other_playing(self):
        first, second = self.browser('one'), self.browser('two')
        await self.reply(first, 'shared')
        revision = second.revision
        self.assertTrue(second.browser_cancelled('shared', revision, True))
        utterance = self.hub.utterances['shared']
        self.assertEqual(utterance.clients['two']['status'], 'interrupted')
        self.assertIsNone(second.active)
        # The other browser is untouched, and so is the shared message.
        self.assertEqual(utterance.clients['one']['status'], 'synthesizing')
        self.assertEqual(first.active, 'shared')
        self.assertEqual(self.row('shared', first.id)['text'], 'Ya está listo')
        self.assertEqual(self.row('shared', first.id)['status'], 'synthesizing')
        first.transition('shared', 'playing')
        await first.playback_finished('shared', revision)
        self.assertEqual(self.row('shared', first.id)['status'], 'playback_finished')

    async def test_a_failure_in_one_browser_does_not_silence_the_others(self):
        first, second = self.browser('one'), self.browser('two')
        await self.reply(first, 'shared')
        second.fail_active()
        self.assertEqual(self.hub.utterances['shared'].clients['two']['status'], 'failed')
        self.assertEqual(self.hub.utterances['shared'].clients['one']['status'], 'synthesizing')
        self.assertEqual(self.row('shared', first.id)['status'], 'synthesizing')

    async def test_a_receipt_from_one_browser_can_never_move_another(self):
        first, second = self.browser('one'), self.browser('two')
        await self.reply(first, 'shared')
        revision = second.revision
        # Whatever the second browser claims, it is answered under its own id.
        second.browser_cancelled('shared', revision, True)
        self.assertFalse(second.is_current('shared', revision))
        self.assertTrue(first.is_current('shared', revision))
        # A stale epoch from the second browser is refused outright.
        second.user_started(); second.speaking = False
        self.assertFalse(second.browser_cancelled('shared', revision + 5, True))
        await second.playback_finished('shared', revision)
        self.assertEqual(self.hub.utterances['shared'].clients['two']['status'], 'interrupted')

    # ----- input -----

    async def test_each_browser_input_is_delivered_once_and_overlapping_turns_are_ordered(self):
        first, second = self.browser('one'), self.browser('two')
        first.user_started()          # each browser's turn advances its own epoch only
        second.user_started()
        self.assertEqual((first.turn_revision, second.turn_revision), (1, 1))
        first.speaking = second.speaking = False
        first.enqueue_input('Lo que dije yo')
        second.enqueue_input('Lo que dijo el otro')
        first.enqueue_input('Lo que dije yo')   # a redelivery of the same turn
        rows = self.hub.journal.pending()
        self.assertEqual([row['text'] for row in rows], ['Lo que dije yo', 'Lo que dijo el otro'])
        self.assertEqual([row['session'] for row in rows], ['one', 'two'])
        self.assertEqual([row['revision'] for row in rows], [1, 1])
        self.assertEqual(len({row['id'] for row in rows}), 2)

    async def test_the_outbox_delivers_each_participant_exactly_once_in_order(self):
        from sidevoice.connector_control import ConnectorControl
        from test_connector_control import FakePeer
        control = ConnectorControl(self.hub.journal, self.hub)
        self.addCleanup(lambda: [control.drop_inflight(b) for b in list(control.inflight)])
        peer = FakePeer()
        binding = self.hub.journal.register_binding('conn-1', harness='test', thread='task')
        control.peers['conn-1'] = peer; control.live[binding['id']] = 'conn-1'
        first, second = self.browser('one'), self.browser('two')
        for client, text in ((first, 'De la primera'), (second, 'De la segunda')):
            client.user_started(); client.speaking = False
            client.enqueue_input(text)
        for turn in (1, 2):
            await control.tick(); await peer.until_asked(turn)
            await control.acknowledge('conn-1', peer.asked[-1][1]['event_id'], {'status': 'accepted'})
        await control.tick()
        self.assertEqual(peer.deliveries('text'), ['De la primera', 'De la segunda'])
        self.assertEqual(len(set(peer.deliveries('event_id'))), 2)
        self.assertEqual(peer.deliveries('session_id'), ['one', 'two'])

    async def test_a_receipt_reaches_the_browser_that_spoke_and_no_other(self):
        first, second = self.browser('one'), self.browser('two')
        first.receipts, second.receipts = [], []
        first.on_input_receipt = first.receipts.append
        second.on_input_receipt = second.receipts.append
        first.user_started(); first.speaking = False
        row = first.enqueue_input('Solo mío')
        self.hub.delivery_status(row['history_id'], 'delivered')
        self.assertEqual([r['status'] for r in first.receipts], ['pending', 'delivered'])
        self.assertEqual(second.receipts, [])
        self.assertEqual((first.sent, second.sent), (1, 0))
        # And the latency trace of the turn belongs to the browser that took it.
        self.assertIn(('task', first.turn_revision), first.latency.turns)
        self.assertEqual(second.latency.turns, {})

    async def test_a_typed_message_does_not_cut_the_reply_that_is_playing(self):
        # Asked for on 2026-09-21: pasting a text while a reply plays must not stop it (#67).
        client = self.browser('one')
        await self.reply(client, 'sounding', text='Te estoy contando algo')
        client.transition('sounding', 'playing')
        before = client.revision
        await self.hub.send_text('Te paso esto mientras', client.id, 'task', client.target['binding_id'], 'typed-1')
        self.assertEqual(client.revision, before)
        self.assertEqual(self.hub.utterances['sounding'].clients['one']['status'], 'playing')
        self.assertNotIn('voice-cancel', [event['type'] for event in client.heard])
        await client.playback_finished('sounding', before)
        self.assertEqual(self.hub.utterances['sounding'].clients['one']['status'], 'playback_finished',
                         'its ending still counts: the reply was never made stale')
        # A reply to the typed text is as current as any other.
        await self.hub.speak('Recibido', 'answer', client.id, before)

    async def test_typed_input_opens_a_room_turn_without_taking_over_another_microphone(self):
        first, second = self.browser('one'), self.browser('two')
        second.user_started()
        microphone_turn = second.turn_revision
        result = await self.hub.send_text('Escribo yo', first.id, 'task',
                                          first.target['binding_id'], 'msg-1')
        self.assertEqual(result['revision'], first.revision)
        self.assertEqual(first.revision, 0, 'typing is not a new epoch: it interrupts nothing (#67)')
        self.assertEqual(second.turn_revision, microphone_turn)
        self.assertTrue(second.speaking)
        row = self.hub.journal.get(result['id'])
        self.assertEqual((row['session'], row['text']), ('one', 'Escribo yo'))
        with self.assertRaises(HTTPException):
            # Nobody may send text as a browser that is not theirs.
            await self.hub.send_text('Suplantando', 'nadie', 'task',
                                     first.target['binding_id'], 'msg-2')

    # ----- coming and going -----

    async def test_one_browser_leaving_keeps_the_room_and_everyone_else_live(self):
        first, second = self.browser('one'), self.browser('two')
        await self.reply(first, 'shared')
        revision = second.revision
        first.disconnect()
        self.assertFalse(first.connected)
        self.assertEqual(list(self.hub.clients), ['two'])
        self.assertTrue(second.connected)
        self.assertEqual(second.revision, revision)
        self.assertEqual(second.target['thread_id'], 'task')
        self.assertEqual(self.hub.utterances['shared'].clients['one']['status'], 'disconnected')
        # The row still reports the browser that is actually still playing it.
        self.assertEqual(self.row('shared', first.id)['status'], 'synthesizing')
        await second.playback_finished('shared', revision)
        self.assertEqual(self.row('shared', first.id)['status'], 'playback_finished')
        # And the room keeps accepting work from whoever is left.
        second.user_started(); second.speaking = False
        second.enqueue_input('Sigo aquí')
        self.assertEqual(self.hub.journal.pending()[0]['text'], 'Sigo aquí')

    async def test_a_browser_that_left_is_not_a_listener_and_nothing_is_replayed_into_the_next(self):
        first = self.browser('one')
        first.disconnect()
        result = await self.hub.publish(Speech(thread_id='task', session_id='one',
                                               revision=first.revision, text='Nadie escucha',
                                               utterance_id='alone'))
        self.assertEqual((result['status'], result['reason']), ('text_only', 'call_ended'))
        self.assertTrue(result['text_saved'])
        second = self.browser('two')
        self.assertEqual(self.spoken(second), [])

    async def test_a_browser_swapping_pipelines_holds_two_clients_and_the_one_it_drops_takes_nothing(self):
        """One device, two sockets, for as long as a settings change takes.

        The second is a client like any other: its own id, its own selection, its own epoch. When
        the first goes, its playback entries settle as `disconnected` and the second keeps playing
        the same utterance, because nothing in the room was ever shared between the two.
        """
        old, new = self.browser('before'), self.browser('after')
        await self.reply(old, 'shared')
        self.assertEqual(set(self.hub.utterances['shared'].clients), {'before', 'after'})

        old.disconnect()
        self.assertEqual(list(self.hub.clients), ['after'])
        self.assertEqual(self.hub.utterances['shared'].clients['before']['status'], 'disconnected')
        self.assertEqual(self.hub.utterances['shared'].clients['after']['status'], 'synthesizing',
                         'the session that replaced it keeps playing what it was given')
        self.assertEqual(new.target['thread_id'], 'task', 'the selection travelled in the new hello, not from the old client')
        # The row follows the browser that is actually still playing it.
        self.assertEqual(self.row('shared', old.id)['status'], 'synthesizing')
        await new.playback_finished('shared', new.revision)
        self.assertEqual(self.row('shared', old.id)['status'], 'playback_finished')

        # And the new session's epoch is its own: the one it replaced never moved it.
        new.user_started(); new.speaking = False
        self.assertEqual((new.revision, new.turn_revision), (1, 1))
        self.assertEqual(old.revision, 0)
        new.enqueue_input('Sigo aquí con los ajustes nuevos')
        self.assertEqual(self.hub.journal.pending()[-1]['text'], 'Sigo aquí con los ajustes nuevos')
        self.assertEqual(self.hub.journal.pending()[-1]['session'], 'after')

    async def test_the_room_refuses_more_browsers_than_it_bounds_without_dropping_any(self):
        clients = [self.browser('client-%d' % index) for index in range(self.hub.max_clients)]
        with self.assertRaises(RuntimeError):
            self.browser('one-too-many')
        self.assertEqual(len(self.hub.clients), self.hub.max_clients)
        self.assertTrue(all(client.connected for client in clients))

    async def test_the_room_says_whether_it_would_admit_a_browser_and_why_not(self):
        # A refusal travels in a frame and in a close code, and a proxy loses both: this is the answer
        # the page that was refused asks for over plain HTTP (#63).
        free = self.hub.admission()
        self.assertEqual((free['admitted'], free['reason'], free['message']), (True, None, None))
        self.assertEqual((free['clients'], free['max']), (len(self.hub.clients), self.hub.max_clients))
        while len(self.hub.clients) < self.hub.max_clients:
            self.browser('client-%d' % len(self.hub.clients))
        full = self.hub.admission()
        self.assertEqual((full['admitted'], full['reason']), (False, 'room_is_full'))
        self.assertEqual(full['message'], self.hub.FULL_MESSAGE, 'one sentence, written in one place')
        with self.assertRaises(RuntimeError) as refused:
            self.browser('one-too-many')
        self.assertEqual(str(refused.exception), full['message'], 'and the socket refuses with that same one')

    async def test_how_many_browsers_a_room_carries_is_the_machine_s_to_say(self):
        from sidevoice.room import MAX_BROWSERS, browser_limit
        self.assertEqual(browser_limit({}), MAX_BROWSERS)
        self.assertEqual(browser_limit({'VOICE_MAX_BROWSERS': '3'}), 3)
        self.assertEqual(browser_limit({'VOICE_MAX_BROWSERS': 'unas cuantas'}), MAX_BROWSERS,
                         'an unreadable limit is the default, never no limit at all')
        self.assertEqual(browser_limit({'VOICE_MAX_BROWSERS': '0'}), 1, 'a room nobody may enter is not a setting')
        # A room built on a machine that said how many it can carry holds that number, and everything
        # that speaks about the limit — the refusal, the admission answer — speaks about that one.
        with patch.dict(os.environ, {'VOICE_MAX_BROWSERS': '2'}):
            small = Room(RoomHistory(self.path('small.sqlite3')))
        self.assertEqual(small.max_clients, 2)
        pair = [RoomClient('client-%d' % index, small, worker=AsyncMock()) for index in range(2)]
        with self.assertRaises(RuntimeError) as refused:
            RoomClient('one-too-many', small, worker=AsyncMock())
        self.assertEqual(str(refused.exception), small.FULL_MESSAGE)
        self.assertEqual(small.admission(), {'admitted': False, 'reason': 'room_is_full',
                                             'message': small.FULL_MESSAGE, 'clients': 2, 'max': 2})
        self.assertEqual(list(small.clients), [client.id for client in pair], 'and neither of them was dropped')

    # ----- audio the room pays for -----

    async def test_paid_synthesis_happens_once_per_utterance_and_is_fanned_out(self):
        self.voice = ELEVEN
        first, second, third = self.browser('one'), self.browser('two'), self.browser('three')
        await self.reply(first, 'shared', text='Esto lo paga la sala una sola vez')
        self.assertEqual(self.renders, [('elevenlabs', 'una-voz', 'Esto lo paga la sala una sola vez')])
        for client in (first, second, third):
            spoken = self.spoken(client)
            self.assertEqual(len(spoken), 1)
            self.assertEqual(spoken[0]['audio_base64'], 'YQ==')
            # Karaoke needs the same alignment everywhere, and it costs no second request.
            self.assertEqual(spoken[0]['alignment'], {'characters': ['E', 's', 't']})
        self.assertEqual(self.hub.assets.stats()['renders'], 1)
        self.assertEqual(self.hub.assets.stats()['reuses'], 2)
        # A later utterance with the same words and voice is not paid for twice either.
        for client in (first, second, third):
            client.transition('shared', 'playing')
            await client.playback_finished('shared', client.revision)
        await self.reply(first, 'again', text='Esto lo paga la sala una sola vez')
        self.assertEqual(len(self.renders), 1)

    async def test_a_shared_render_is_never_reported_as_a_wait_the_listener_did_not_make(self):
        self.voice = ELEVEN
        first, second = self.browser('one'), self.browser('two')
        await self.reply(first, 'shared')
        payer, reuser = self.spoken(first)[0], self.spoken(second)[0]
        self.assertEqual(payer['timings_ms'], {'request_to_complete_ms': 20})
        self.assertFalse(payer['shared'])
        self.assertEqual(reuser['timings_ms'], {})
        self.assertTrue(reuser['shared'])
        self.assertEqual(first.latency.snapshot()['replies'][0]['provider_ms'],
                         {'request_to_complete_ms': 20})
        self.assertEqual(second.latency.snapshot()['replies'][0]['provider_ms'], {})

    async def test_a_slow_render_is_shared_in_flight_rather_than_repeated(self):
        self.voice = ELEVEN
        gate, asked = asyncio.Event(), []
        async def slow(choice, text):
            asked.append(text)
            await gate.wait()
            return {'mime_type': 'audio/mpeg', 'audio_base64': 'YQ==', 'timings_ms': {}, 'alignment': None}
        self.hub.assets.renderer = slow
        clients = [self.browser('one'), self.browser('two'), self.browser('three')]
        reply = asyncio.create_task(self.reply(clients[0], 'shared'))
        for _ in range(8):
            await asyncio.sleep(0)   # every browser reaches the render before any of them finishes
        self.assertEqual(asked, ['Ya está listo'])
        gate.set()
        await reply
        self.assertEqual(self.hub.assets.stats()['renders'], 1)
        for client in clients:
            self.assertEqual(len(self.spoken(client)), 1)

    async def test_a_paid_engine_that_fails_marks_the_browsers_and_keeps_the_message(self):
        self.voice = ELEVEN
        async def broken(choice, text):
            raise ValueError('ElevenLabs respondió 429.')
        self.hub.assets.renderer = broken
        first, second = self.browser('one'), self.browser('two')
        await self.reply(first, 'shared')
        for client in (first, second):
            self.assertEqual(self.spoken(client), [])
            self.assertIn('429', client.error)
            self.assertEqual(self.hub.utterances['shared'].clients[client.id]['status'], 'failed')
        # The text is the room's and survives a provider outage.
        self.assertEqual(self.row('shared', first.id)['text'], 'Ya está listo')

    async def test_a_snapshot_shows_the_room_to_all_and_playback_only_to_its_owner(self):
        first, second = self.browser('one'), self.browser('two')
        await self.reply(first, 'shared')
        second.browser_cancelled('shared', second.revision, True)
        mine = self.hub.snapshot('one')
        self.assertEqual(mine['call']['id'], 'one')
        self.assertEqual(mine['call']['utterances'], [{'utterance_id': 'shared', 'revision': first.revision,
                                                       'session_id': 'one', 'status': 'synthesizing'}])
        self.assertEqual(mine['room']['clients'], 2)
        self.assertEqual({entry['id'] for entry in mine['clients']}, {'one', 'two'})
        theirs = self.hub.snapshot('two')
        self.assertEqual(theirs['call']['utterances'][0]['status'], 'interrupted')
        # An unknown browser is told about the room, never handed someone else's playback.
        self.assertIsNone(self.hub.snapshot('who')['call'])
        self.assertIsNone(self.hub.snapshot()['call'])


class SharedRenderTests(IsolatedAsyncioTestCase):
    """The cache itself: one render per configuration, shared, bounded."""

    def choice(self, voice='v', speed=1.0):
        return {'provider': 'elevenlabs', 'model': 'm', 'voice': voice, 'speed': speed}

    @staticmethod
    def audio(size=4):
        return {'mime_type': 'audio/mpeg', 'audio_base64': 'a' * size, 'timings_ms': {}, 'alignment': None}

    async def test_concurrent_listeners_share_one_in_flight_render(self):
        started, release, calls = asyncio.Event(), asyncio.Event(), []
        async def slow(choice, text):
            calls.append(text)
            started.set()
            await release.wait()
            return self.audio()
        cache = SynthesisCache(renderer=slow)
        waiting = [asyncio.create_task(cache.obtain(self.choice(), 'hola')) for _ in range(4)]
        await started.wait()
        release.set()
        results = await asyncio.gather(*waiting)
        self.assertEqual(calls, ['hola'])
        self.assertEqual({id(audio) for audio, _ in results}, {id(results[0][0])})
        self.assertEqual([fresh for _, fresh in results].count(True), 1)
        self.assertEqual(cache.stats()['renders'], 1)

    async def test_a_listener_that_walks_away_does_not_cancel_the_render_for_the_others(self):
        started, release = asyncio.Event(), asyncio.Event()
        async def slow(choice, text):
            started.set()
            await release.wait()
            return self.audio()
        cache = SynthesisCache(renderer=slow)
        leaving = asyncio.create_task(cache.obtain(self.choice(), 'hola'))
        await started.wait()
        staying = asyncio.create_task(cache.obtain(self.choice(), 'hola'))
        await asyncio.sleep(0)
        leaving.cancel()
        release.set()
        audio, fresh = await staying
        self.assertEqual(audio['audio_base64'], 'aaaa')
        self.assertFalse(fresh)
        self.assertEqual(cache.stats()['renders'], 1)

    async def test_the_key_separates_anything_that_would_change_the_audio(self):
        calls = []
        async def renderer(choice, text):
            calls.append((choice['voice'], choice['speed'], text))
            return self.audio()
        cache = SynthesisCache(renderer=renderer)
        await cache.obtain(self.choice(), 'hola')
        await cache.obtain(self.choice(), 'hola')
        await cache.obtain(self.choice(voice='otra'), 'hola')
        await cache.obtain(self.choice(speed=1.2), 'hola')
        await cache.obtain(self.choice(), 'adiós')
        self.assertEqual(len(calls), 4)
        self.assertEqual(cache.stats()['reuses'], 1)

    async def test_the_cache_is_bounded_and_drops_the_least_recently_used(self):
        async def renderer(choice, text):
            return {**self.audio(), 'audio_base64': text}
        cache = SynthesisCache(limit_items=2, renderer=renderer)
        first, _ = await cache.obtain(self.choice(), 'uno')
        await cache.obtain(self.choice(), 'dos')
        await cache.obtain(self.choice(), 'uno')      # keeps the first one warm
        await cache.obtain(self.choice(), 'tres')
        self.assertIsNotNone(cache.read(SynthesisCache.key(self.choice(), 'uno')))
        self.assertEqual(cache.stats()['items'], 2)
        self.assertEqual(cache.stats()['bytes'], len('uno') + len('tres'))
        self.assertIs(cache.read(SynthesisCache.key(self.choice(), 'uno')), first)

    async def test_bytes_are_bounded_too(self):
        async def renderer(choice, text):
            return self.audio(100)
        cache = SynthesisCache(limit_bytes=250, renderer=renderer)
        for word in ('uno', 'dos', 'tres', 'cuatro'):
            await cache.obtain(self.choice(), word)
        self.assertEqual(cache.stats()['items'], 2)
        self.assertLessEqual(cache.stats()['bytes'], 250)


class PerBrowserSelectionTests(IsolatedAsyncioTestCase):
    """Which conversation a browser talks to is that browser's state; the room only routes."""

    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.hub = Room(RoomHistory(Path(self.temp.name) / 'history.sqlite3'), SynthesisCache(renderer=self.render))
        voices = patch('sidevoice.language_settings.resolve_voice', side_effect=lambda *a, **k: dict(KOKORO))
        voices.start(); self.addCleanup(voices.stop)
        for thread, title in (('a', 'Conversación A'), ('b', 'Conversación B')):
            self.hub.journal.register_binding('conn', harness='claude', thread=thread, title=title)

    async def render(self, choice, text):
        return {'mime_type': 'audio/mpeg', 'audio_base64': 'YQ==', 'timings_ms': {}, 'alignment': {'characters': []}}

    def browser(self, session_id):
        client = RoomClient(session_id, self.hub, worker=AsyncMock())
        client.connected = True
        client.heard = []
        client.on_browser_event = client.heard.append
        return client

    async def test_selecting_in_one_browser_moves_nobody_else_and_each_hears_its_own_conversation(self):
        one, two = self.browser('one'), self.browser('two')
        self.assertIsNone(self.hub.snapshot('one')['binding'])
        await self.hub.select('one', 'a', 'Conversación A')
        await self.hub.select('two', 'b', 'Conversación B')
        self.assertEqual(self.hub.snapshot('one')['binding']['thread_id'], 'a')
        self.assertEqual(self.hub.snapshot('two')['binding']['thread_id'], 'b')
        # Switching one tab interrupts that tab only; the other keeps its epoch and its selection.
        before, heard = two.revision, len(two.heard)
        await self.hub.select('one', 'b', 'Conversación B')
        self.assertEqual(two.revision, before)
        self.assertEqual(two.target['thread_id'], 'b')
        self.assertEqual(one.heard[-1]['type'], 'voice-cancel')
        self.assertEqual(len(two.heard), heard, 'the other tab heard nothing about it')
        # A reply for B reaches every browser on B and no browser elsewhere.
        await self.hub.select('one', 'a', 'Conversación A')
        two.user_started(); two.speaking = False
        result = await self.hub.publish(Speech(thread_id='b', session_id='two', revision=two.revision, text='Para B', utterance_id='u-b'))
        self.assertIn(result['status'], {'queued', 'synthesizing'})
        self.assertEqual(set(self.hub.utterances['u-b'].clients), {'two'})
        # A reply is judged by the epoch of the browser it answers, not by anyone else's turns.
        one.user_started(); one.speaking = False
        self.assertIn(self.hub.utterances['u-b'].clients['two']['status'], {'queued', 'synthesizing'}, "untouched by another tab's turn")
        # A reply for a conversation this tab is not on is text only for it, whatever another tab is doing.
        elsewhere = await self.hub.publish(Speech(thread_id='b', session_id='one', revision=one.revision, text='Para B otra vez', utterance_id='u-b2'))
        self.assertEqual((elsewhere['status'], elsewhere['reason']), ('text_only', 'focus_changed'))

    async def test_input_goes_to_the_browsers_own_conversation_and_text_needs_its_own_selection(self):
        one, two = self.browser('one'), self.browser('two')
        await self.hub.select('one', 'a'); await self.hub.select('two', 'b')
        one.user_started(); one.speaking = False; one.enqueue_input('Para A')
        two.user_started(); two.speaking = False; two.enqueue_input('Para B')
        self.assertEqual([(row['session'], row['thread']) for row in self.hub.journal.pending()], [('one', 'a'), ('two', 'b')])
        with self.assertRaises(HTTPException):
            await self.hub.send_text('Ajeno', 'one', 'b', two.target['binding_id'], 'm-1')
        result = await self.hub.send_text('Escrito para A', 'one', 'a', one.target['binding_id'], 'm-2')
        self.assertEqual(result['revision'], one.revision)
        self.assertEqual(self.hub.journal.get(result['id'])['thread'], 'a')

    async def test_closing_a_conversation_clears_it_only_in_the_browsers_that_had_it(self):
        one, two = self.browser('one'), self.browser('two')
        await self.hub.select('one', 'a'); await self.hub.select('two', 'b')
        await self.hub.close_channel('a')
        self.assertIsNone(self.hub.snapshot('one')['binding'])
        self.assertEqual(self.hub.snapshot('two')['binding']['thread_id'], 'b')
        self.assertIsNone(self.hub.journal.binding_for_thread('a'))
        one.user_started(); one.speaking = False; one.enqueue_input('Sin destino')
        self.assertIn('Select a conversation', one.error)

    async def test_deselecting_needs_the_browsers_current_binding_and_touches_only_that_browser(self):
        one, two = self.browser('one'), self.browser('two')
        await self.hub.select('one', 'a'); await self.hub.select('two', 'a')
        with self.assertRaises(HTTPException):
            await self.hub.deselect('one', two.target['binding_id'])
        await self.hub.deselect('one', one.target['binding_id'])
        self.assertIsNone(self.hub.snapshot('one')['binding'])
        self.assertEqual(two.target['thread_id'], 'a')


class ReplayOnReturnTests(RoomFixture):
    """What a browser is played when it comes back, and what it is never played again (#52).

    Driving through a tunnel drops the socket; the transcript keeps the text and a driver cannot read
    it. Everything here asks the same question: does the room offer exactly the replies *this* browser
    never heard through, in the order they were said, and does it stop the moment the person speaks?
    """

    async def heard_to_the_end(self, client, utterance_id):
        client.transition(utterance_id, 'playing')
        await client.playback_finished(utterance_id, client.revision)

    async def returning(self, session_id, *, sessions=(), seconds=120):
        """The same tab after a reconnection: a new client id naming the ids it used before."""
        client = self.browser(session_id)
        return client, await self.hub.replay(client, seconds=seconds, sessions=sessions)

    def announcement(self, client):
        return [event['data'] for event in client.heard if event['type'] == 'voice-replay']

    async def test_a_browser_that_comes_back_hears_what_it_missed_oldest_first_and_nothing_it_finished(self):
        first = self.browser('one')
        await self.reply(first, 'heard', text='La primera')
        await self.heard_to_the_end(first, 'heard')
        await self.reply(first, 'cut', text='La segunda')
        await self.reply(first, 'never', text='La tercera')
        first.disconnect()   # the tunnel: the socket goes and the call does not
        back, summary = await self.returning('back', sessions=['one'])
        self.assertEqual([item['history_id'] for item in summary['replayed']],
                         ['one:voice:cut', 'one:voice:never'], 'oldest first, and only what it missed')
        self.assertEqual(summary['skipped'], [])
        # The browser is told before any of it sounds, so the bubbles can say they are repetitions.
        self.assertEqual(self.announcement(back)[0]['replies'], summary['replayed'])
        spoken = self.spoken(back)
        self.assertEqual([item['text'] for item in spoken], ['La segunda'], 'one at a time, like any reply')
        self.assertEqual((spoken[0]['replay'], spoken[0]['history_id']), (True, 'one:voice:cut'))
        self.assertEqual(spoken[0]['revision'], back.revision, "at the epoch of the browser hearing it")
        # The queue holds the rest; nothing about the room's own utterances moved.
        self.assertEqual(list(back.pending), ['never:replay:back'])
        self.assertEqual(self.hub.utterances['cut'].clients['one']['status'], 'disconnected')

    async def test_a_reply_this_listener_stopped_is_not_repeated_to_it(self):
        first = self.browser('one')
        await self.reply(first, 'stopped', text='La que paraste')
        first.transition('stopped', 'playing')
        first.browser_cancelled('stopped', first.revision, True)
        first.disconnect()
        _, summary = await self.returning('back', sessions=['one'])
        self.assertEqual(summary['replayed'], [], 'stopping the audio is a decision, not a gap')

    async def test_another_browsers_playback_never_answers_for_this_one(self):
        first, other = self.browser('one'), self.browser('two')
        await self.reply(first, 'shared', text='Para los dos')
        await self.heard_to_the_end(other, 'shared')
        first.disconnect()
        _, mine = await self.returning('back', sessions=['one'])
        self.assertEqual([item['history_id'] for item in mine['replayed']], ['one:voice:shared'])
        # And a browser that names the session which did hear it is told nothing.
        _, theirs = await self.returning('other-back', sessions=['two'])
        self.assertEqual(theirs['replayed'], [])

    async def test_a_new_turn_cancels_the_catch_up_and_it_is_never_held_for_later(self):
        first = self.browser('one')
        await self.reply(first, 'one-missed', text='Primera')
        await self.reply(first, 'two-missed', text='Segunda')
        first.disconnect()
        back, summary = await self.returning('back', sessions=['one'])
        self.assertEqual(len(summary['replayed']), 2)
        back.user_started()
        self.assertEqual(list(back.pending), [], 'a catch-up is dropped by a turn, not queued behind it')
        self.assertEqual(self.hub.utterances['two-missed:replay:back'].clients['back']['status'], 'interrupted')
        # It never sounded, so it is still a reply this browser has not heard: the next return offers it.
        _, again = await self.returning('back-again', sessions=['one', 'back'])
        self.assertEqual([item['history_id'] for item in again['replayed']],
                         ['one:voice:one-missed', 'one:voice:two-missed'])

    async def test_a_catch_up_played_to_the_end_is_not_repeated_on_the_next_return(self):
        first = self.browser('one')
        await self.reply(first, 'missed', text='Lo que no oíste')
        first.disconnect()
        back, _ = await self.returning('back', sessions=['one'])
        await self.heard_to_the_end(back, 'missed:replay:back')
        back.disconnect()
        _, again = await self.returning('back-again', sessions=['one', 'back'])
        self.assertEqual(again['replayed'], [], 'it was heard through; the room does not say it twice')

    async def test_the_recency_window_is_the_devices_and_off_means_nothing_is_repeated(self):
        first = self.browser('one')
        await self.reply(first, 'old', text='Hace rato')
        await self.reply(first, 'recent', text='Hace nada')
        self.hub.utterances['old'].at -= 600
        first.disconnect()
        _, narrow = await self.returning('a', sessions=['one'], seconds=120)
        self.assertEqual([item['history_id'] for item in narrow['replayed']], ['one:voice:recent'])
        _, wide = await self.returning('b', sessions=['one'], seconds=900)
        self.assertEqual([item['history_id'] for item in wide['replayed']],
                         ['one:voice:old', 'one:voice:recent'])
        off, summary = await self.returning('c', sessions=['one'], seconds=0)
        self.assertEqual((summary['replayed'], summary['skipped'], self.announcement(off)), ([], [], []))

    async def test_a_catch_up_writes_nothing_in_the_journal_until_it_has_actually_sounded(self):
        first = self.browser('one')
        await self.reply(first, 'missed', text='Lo que no oíste')
        first.disconnect()
        self.assertEqual(self.row('missed', 'one')['status'], 'disconnected')
        back, _ = await self.returning('back', sessions=['one'])
        self.assertEqual(len(self.hub.journal.history('task')), 1, 'a repetition is not a second message')
        self.assertEqual(self.row('missed', 'one')['status'], 'disconnected',
                         'queueing a repetition says nothing about the reply yet')
        await self.heard_to_the_end(back, 'missed:replay:back')
        self.assertEqual(self.row('missed', 'one')['status'], 'playback_finished',
                         'the row carries the furthest any listener got, and now someone got to the end')

    async def test_a_reply_nobody_was_there_to_hear_waits_for_the_first_who_returns(self):
        # Published against a session this room no longer knows (a restart, a reload), with nobody on the
        # conversation: it used to be text for ever. It is a first delivery delayed, so it is kept and the
        # returning browser hears it through the ordinary replay — rendered now, for the first time (#17).
        self.voice = ELEVEN
        result = await self.hub.publish(Speech(thread_id='task', session_id='ghost', revision=3,
                                               text='Llegó cuando no estabas', utterance_id='parked'))
        self.assertEqual((result['status'], result['reason']), ('text_only', 'session_changed'))
        self.assertTrue(self.hub.utterances['parked'].parked)
        self.assertEqual(self.renders, [], 'nothing is rendered for nobody')
        back, summary = await self.returning('back', sessions=[])
        self.assertEqual([item['history_id'] for item in summary['replayed']], ['ghost:voice:parked'])
        self.assertEqual(summary['skipped'], [], 'a reply never bought is bought once, not refused as gone')
        await self.heard_to_the_end(back, 'parked:replay:back')
        self.assertEqual(self.renders, [('elevenlabs', 'una-voz', 'Llegó cuando no estabas')])
        self.assertEqual(self.row('parked', 'ghost')['status'], 'playback_finished')

    async def test_a_paid_render_the_room_no_longer_has_is_said_and_never_bought_again(self):
        self.voice = ELEVEN
        first = self.browser('one')
        await self.reply(first, 'kept', text='Con audio')
        await self.reply(first, 'dropped', text='Sin audio')
        first.disconnect()
        # Both were paid for; then the bounded cache drops the second one.
        await self.hub.assets.obtain(ELEVEN, 'Sin audio')
        self.hub.utterances['dropped'].rendered = True
        self.hub.assets.entries.pop(self.hub.assets.key(ELEVEN, 'Sin audio'))
        self.renders.clear()
        back, summary = await self.returning('back', sessions=['one'])
        self.assertEqual([item['history_id'] for item in summary['replayed']], ['one:voice:kept'])
        self.assertEqual(summary['skipped'], [{'history_id': 'one:voice:dropped', 'reason': 'audio_gone'}])
        self.assertEqual(self.announcement(back)[0]['skipped'], summary['skipped'])
        self.assertEqual(self.renders, [], 'repeating what someone missed never bills the account again')
        self.assertEqual(self.spoken(back)[0]['shared'], True)

    async def test_a_reply_still_queued_when_the_person_left_was_never_bought_and_plays_on_return(self):
        # Seen on 2026-09-26: a reply published while the person was still in the call, queued behind
        # another, and never reached before they left, came back as "la sala ya no tiene este audio".
        self.voice = ELEVEN
        first = self.browser('one')
        await self.reply(first, 'kept', text='Con audio')
        await self.reply(first, 'never', text='Nunca sonó')
        first.disconnect()
        self.assertEqual(self.renders, [('elevenlabs', 'una-voz', 'Con audio')])
        back, summary = await self.returning('back', sessions=['one'])
        self.assertEqual([item['history_id'] for item in summary['replayed']], ['one:voice:kept', 'one:voice:never'])
        self.assertEqual(summary['skipped'], [])
        await self.heard_to_the_end(back, 'kept:replay:back')
        await self.heard_to_the_end(back, 'never:replay:back')
        self.assertEqual(self.renders, [('elevenlabs', 'una-voz', 'Con audio'), ('elevenlabs', 'una-voz', 'Nunca sonó')],
                         'bought once, now, and the one already bought is not bought again')

    async def test_only_the_conversation_this_browser_is_on_is_caught_up(self):
        first = self.browser('one')
        await self.reply(first, 'mine', text='De esta conversación')
        first.disconnect()
        elsewhere = self.browser('back')
        elsewhere.target = {'thread_id': 'otra', 'title': 'Otra', 'binding_id': 'b'}
        summary = await self.hub.replay(elsewhere, seconds=120, sessions=['one'])
        self.assertEqual((summary['replayed'], summary['skipped']), ([], []))
        nowhere = self.browser('nada')
        nowhere.target = {}
        self.assertEqual(self.hub.missed_replies(nowhere, seconds=120), [])

    async def test_a_browser_entering_the_conversation_for_the_first_time_hears_what_is_recent(self):
        first = self.browser('one')
        await self.reply(first, 'recent', text='Lo último que te dije')
        _, summary = await self.returning('fresh')
        self.assertEqual([item['history_id'] for item in summary['replayed']], ['one:voice:recent'],
                         'a tab that names no earlier session of its own has heard nothing')


class ReplyAfterTheBrowserChangedTests(IsolatedAsyncioTestCase):
    """A reply answers a person, not a socket: the browser that asked may have been replaced meanwhile."""

    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.hub = Room(RoomHistory(Path(self.temp.name) / 'history.sqlite3'), SynthesisCache(renderer=self.render))
        voices = patch('sidevoice.language_settings.resolve_voice', side_effect=lambda *a, **k: dict(KOKORO))
        voices.start(); self.addCleanup(voices.stop)
        self.hub.journal.register_binding('conn', harness='claude', thread='a', title='A')

    async def render(self, choice, text):
        return {'mime_type': 'audio/mpeg', 'audio_base64': 'YQ==', 'timings_ms': {}, 'alignment': {'characters': []}}

    def browser(self, session_id):
        client = RoomClient(session_id, self.hub, worker=AsyncMock())
        client.connected = True
        client.target = {'thread_id': 'a', 'title': 'A', 'binding_id': 'bind-' + session_id}
        client.heard = []
        client.on_browser_event = client.heard.append
        return client

    async def test_a_reply_for_a_browser_that_was_replaced_is_played_by_the_one_that_took_its_place(self):
        gone = self.browser('old')
        gone.user_started(); gone.speaking = False
        asked_at = gone.revision
        gone.disconnect()
        # The person came back: same conversation, new socket, new epoch.
        back = self.browser('new')
        back.user_started(); back.speaking = False
        result = await self.hub.publish(Speech(thread_id='a', session_id='old', revision=asked_at,
                                               text='Lo que me preguntaste', utterance_id='u'))
        self.assertIn(result['status'], {'queued', 'synthesizing'})
        self.assertEqual(set(self.hub.utterances['u'].clients), {'new'}, 'it sounds where the person is')

    async def test_with_nobody_on_that_conversation_it_is_still_only_text(self):
        gone = self.browser('old')
        gone.disconnect()
        result = await self.hub.publish(Speech(thread_id='a', session_id='old', revision=0,
                                               text='Nadie escucha', utterance_id='alone'))
        self.assertEqual((result['status'], result['reason']), ('text_only', 'call_ended'))
