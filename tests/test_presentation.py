import asyncio
from unittest import IsolatedAsyncioTestCase
from unittest.mock import AsyncMock, patch
from fastapi import HTTPException
from pipecat.frames.frames import BotStoppedSpeakingFrame, TTSAudioRawFrame
from pipecat.processors.frame_processor import FrameDirection
from sidevoice.presentation import PresentationBoundary, PresentationPlayback, PresentationGate, PresentationSpeech
from sidevoice.room import Room, RoomClient


def room_with(target=None, journal=None):
    room = Room(journal)
    room.default_target = dict(target or {})   # what each browser joining in a test starts pointed at
    return room


def joined(room, session_id='call', *, browser=False, **options):
    client = RoomClient(session_id, room, worker=AsyncMock(), tts=object(), stt=object(), **options)
    client.connected = True
    client.target = dict(getattr(room, 'default_target', {}))
    if browser:
        client.on_browser_event = lambda event: None
    return client


class PresentationTests(IsolatedAsyncioTestCase):
    def call(self, session_id='call'):
        return joined(room_with({'thread_id': 'task'}), session_id)

    async def test_speech_enqueues_once_and_does_not_wait_for_playback(self):
        c = self.call()
        # The stand-in worker never plays anything, so a speak that waited for playback would never
        # return. The timeout only guards against that hang; a tenth of a second also caught a full
        # garbage collection over the suite's heap (#69).
        result = await asyncio.wait_for(c.room.speak('Hola', 'u', 'call', 0), 5)
        self.assertEqual(result['status'], 'queued')
        self.assertEqual(await c.room.speak('Hola', 'u', 'call', 0), result)
        c.worker.queue_frames.assert_awaited_once()
        with self.assertRaises(HTTPException) as e:
            await c.room.speak('Otro', 'u', 'call', 0)
        self.assertEqual(e.exception.status_code, 409)

    async def test_stale_revision_rejected_even_after_user_finishes(self):
        c = self.call()
        await c.room.speak('Hola', 'u', 'call', 0)
        c.user_started(); c.speaking = False
        with self.assertRaises(HTTPException):
            await c.room.speak('Respuesta antigua', 'v', 'call', 0)
        self.assertEqual(c.snapshot()['utterances'][0]['status'], 'interrupted')
        await c.playback_finished('u', 0)
        self.assertEqual(c.snapshot()['utterances'][0]['status'], 'interrupted')
        await c.room.speak('Nueva', 'new', 'call', 1)

    async def test_reconnection_does_not_replay_and_requires_new_session(self):
        c = self.call()
        await c.room.speak('Hola', 'u', 'call', 0)
        c.disconnect()
        self.assertEqual(c.snapshot()['utterances'][0]['status'], 'disconnected')
        fresh = self.call('new-call')
        with self.assertRaises(HTTPException):
            await fresh.room.speak('Hola', 'u', 'call', 0)
        fresh.worker.queue_frames.assert_not_awaited()

    async def test_disconnected_or_speaking_never_queues(self):
        c = self.call()
        for connected, speaking in [(False, False), (True, True)]:
            c.connected, c.speaking = connected, speaking
            with self.assertRaises(HTTPException):
                await c.room.speak('Hola', 'new', 'call', 0)
        c.worker.queue_frames.assert_not_awaited()

    async def test_single_dispatch_and_only_ordered_marker_finishes(self):
        c = self.call()
        await c.room.speak('Primera', 'a', 'call', 0)
        await c.room.speak('Segunda', 'b', 'call', 0)
        c.worker.queue_frames.assert_awaited_once()
        p = PresentationPlayback(); p.client = c; p.push_frame = AsyncMock()
        async def emit(f):
            await p.process_frame(f, FrameDirection.DOWNSTREAM)
        await emit(PresentationBoundary(utterance_id='a', revision=0))
        await emit(TTSAudioRawFrame(audio=b'\x01\x00'*100, sample_rate=24000, num_channels=1))
        self.assertEqual(c.utterances['a'].status, 'playing')
        await emit(BotStoppedSpeakingFrame())
        self.assertEqual(c.utterances['a'].status, 'playing')
        await emit(PresentationBoundary(utterance_id='a', revision=0, end=True))
        self.assertEqual(c.utterances['a'].status, 'playback_finished')
        self.assertEqual(c.worker.queue_frames.await_count, 2)

    async def test_pending_and_stale_pipeline_frames_discarded_on_barge_in(self):
        c = self.call()
        await c.room.speak('Primera', 'a', 'call', 0)
        await c.room.speak('Segunda', 'b', 'call', 0)
        c.user_started(); c.speaking = False
        gate = PresentationGate(); gate.client = c; gate.push_frame = AsyncMock()
        await gate.process_frame(PresentationSpeech(text='Primera', utterance_id='a', revision=0), FrameDirection.DOWNSTREAM)
        gate.push_frame.assert_not_awaited()
        await c.playback_finished('a', 0)
        c.worker.queue_frames.assert_awaited_once()
        self.assertFalse(c.pending)

    async def test_failure_cannot_be_overwritten_by_completion(self):
        c = self.call()
        await c.room.speak('Hola', 'u', 'call', 0)
        c.fail_active()
        await c.playback_finished('u', 0)
        self.assertEqual(c.utterances['u'].status, 'failed')

    async def test_input_is_literal_ordered_and_preserves_legitimate_repeats(self):
        c = self.call(); c.user_started(); c.speaking = False
        c.enqueue_input('Vale, para eso.'); c.enqueue_input('Vale, para eso.')
        first, second = c.input_queue.get_nowait(), c.input_queue.get_nowait()
        self.assertEqual(first['text'], 'Vale, para eso.')
        self.assertEqual(first['text'], second['text'])
        self.assertNotEqual(first['message_id'], second['message_id'])
        self.assertEqual((first['thread_id'], first['session_id'], first['revision']), ('task', 'call', 1))

class RoomTests(IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        import tempfile
        from pathlib import Path
        self.temp = tempfile.TemporaryDirectory()
        # Test the default browser provider, never the live room's preferences or paid API.
        from sidevoice.room_history import RoomHistory
        self.hub = Room(RoomHistory(Path(self.temp.name) / 'history.sqlite3'))
        self.c = joined(self.hub, 'same-webrtc')

    async def asyncTearDown(self):
        self.temp.cleanup()

    async def test_empty_room_does_not_buffer_for_later_agent(self):
        receipts = []
        self.c.on_input_receipt = receipts.append
        self.c.user_started()
        self.c.enqueue_input('No hay nadie')
        self.assertTrue(self.c.input_queue.empty())
        self.assertEqual(receipts[-1]['status'], 'not_sent')
        self.assertIn('Select a conversation', self.c.error)
        await self.hub.select(self.c.id, 'a')
        self.c.enqueue_input('Final de la frase anterior')
        self.assertTrue(self.c.input_queue.empty())
        self.c.user_started()
        self.c.enqueue_input('Ahora sí')
        self.assertEqual(self.hub.journal.pending()[0]['thread'], 'a')

    async def test_transfer_preserves_room_and_invalidates_audio_and_turn(self):
        from pipecat.frames.frames import InterruptionFrame
        await self.hub.select(self.c.id, 'a')
        self.c.user_started(); self.c.speaking = False
        previous = self.c.revision
        await self.hub.speak('Largo', 'old', self.c.id, previous)
        self.c.user_started()
        result = await self.hub.select(self.c.id, 'b')
        self.assertEqual(result['binding']['thread_id'], 'b')
        self.assertTrue(self.c.connected)
        self.assertFalse(self.c.closed)
        self.assertEqual(self.c.id, 'same-webrtc')
        self.assertIsInstance(self.c.worker.queue_frame.call_args.args[0], InterruptionFrame)
        self.assertEqual(self.c.utterances['old'].status, 'interrupted')
        # What was said before the switch went to A when the switch closed it (#93); the turn still
        # open is the rest of what the person says, and that is B's.
        self.c.enqueue_input('Y esto ya para B')
        self.assertEqual(self.hub.journal.pending()[0]['thread'], 'b')
        with self.assertRaises(HTTPException):
            await self.hub.speak('Respuesta vieja', 'stale', self.c.id, previous)
        self.c.user_started(); self.c.speaking = False
        self.c.enqueue_input('Nueva para B')
        self.assertEqual(self.hub.journal.pending()[-1]['thread'], 'b')

    async def test_reactivation_idempotent_and_leave_keeps_room(self):
        target = {'thread_id': 'a', 'title': 'A'}
        first = await self.hub.select(self.c.id, target['thread_id'], target['title'])
        revision = self.c.revision
        second = await self.hub.select(self.c.id, target['thread_id'], target['title'])
        self.assertEqual(second['status'], 'already_active')
        self.assertEqual(first['binding'], second['binding'])
        self.assertEqual(self.c.revision, revision)
        await self.hub.deselect(self.c.id, self.c.target['binding_id'])
        self.assertTrue(self.c.connected)
        self.assertFalse(self.c.target.get('thread_id'))
        self.c.user_started(); self.c.enqueue_input('Sala vacía')
        self.assertTrue(self.c.input_queue.empty())

    async def test_browser_dispatch_waits_for_playout_and_cancels_epoch(self):
        room = room_with({'thread_id': 'task'})
        c = joined(room, 'call')
        events = []; c.on_browser_event = events.append
        await room.speak('Hola', 'browser-a', 'call', 0, 'es')
        await room.speak('Hello', 'browser-b', 'call', 0, 'en')
        c.worker.queue_frames.assert_not_awaited()
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]['type'], 'voice-speech')
        self.assertEqual(events[0]['data']['voice'], 'ef_dora')
        self.assertNotIn('final', events[0]['data'])
        await c.playback_finished('browser-a', 0)
        self.assertEqual(events[-1]['data']['language'], 'en')
        c.transition('browser-b', 'playing')
        c.user_started()
        self.assertEqual(events[-1]['type'], 'voice-cancel')
        c.speaking = False
        await c.playback_finished('browser-b', 0)
        self.assertEqual(c.utterances['browser-b'].status, 'interrupted')
        self.assertIsNone(c.active)

    async def test_background_reply_is_kept_as_text_and_replayed_on_return_to_its_conversation(self):
        from sidevoice.presentation import Speech
        await self.hub.select(self.c.id, 'a')
        revision = self.c.revision
        await self.hub.select(self.c.id, 'b')
        payload = Speech(thread_id='a', text='Respuesta para A', session_id=self.c.id,
                         revision=revision, utterance_id='late-a')
        result = await self.hub.publish(payload)
        self.assertEqual(result['status'], 'text_only')
        self.assertTrue(result['text_saved'])
        self.assertEqual(self.hub.journal.history('a')[0]['text'], 'Respuesta para A')
        self.c.worker.queue_frames.assert_not_awaited()
        # Back on A, what was said there while this browser was elsewhere plays, once (#73).
        await self.hub.select(self.c.id, 'a')
        self.c.worker.queue_frames.assert_awaited_once()
        self.assertEqual(self.c.worker.queue_frames.call_args.args[0][1].text, 'Respuesta para A')
        result = await self.hub.publish(payload)
        self.assertEqual(result['status'], 'text_only')
        self.assertEqual(len(self.hub.journal.history('a')), 1, 'a replay writes no second row')
        self.c.worker.queue_frames.assert_awaited_once()

    async def test_pending_inputs_keep_their_original_destination_after_disconnect(self):
        import json
        from pathlib import Path
        from sidevoice.room_history import RoomHistory
        await self.hub.select(self.c.id, 'a')
        self.c.user_started()
        original_revision, original_target = self.c.turn_revision, dict(self.c.turn_target)
        await self.hub.select(self.c.id, 'b')
        # A turn closed by the switch is transcribed afterwards, against the target it was spoken to.
        self.c.enqueue_input('Todavía para A', target=original_target, revision=original_revision)
        self.c.disconnect()
        row = self.hub.journal.pending()[0]
        payload = json.loads(row['payload'])
        self.assertEqual((payload['thread_id'], payload['revision']), ('a', original_revision))
        self.hub.journal.update(row['id'], 'sending')
        self.hub.journal.recover()
        # At-least-once while the room runs: a delivery marked in flight is retried, never abandoned.
        self.assertEqual(len(self.hub.journal.pending()), 1)
        self.assertEqual(self.hub.journal.history()[0]['status'], 'pending')
        # Nothing said is on disk: a new room starts with an empty journal.
        self.assertEqual(RoomHistory(self.hub.journal.path).history(), [])
        self.assertFalse(any(p.suffix.startswith('.sqlite') for p in Path(self.temp.name).iterdir()))

    async def test_identical_words_in_distinct_turns_are_not_deduplicated(self):
        await self.hub.select(self.c.id, 'a')
        for _ in range(2):
            self.c.user_started(); self.c.speaking = False
            self.c.enqueue_input('Sí')
        rows = self.hub.journal.pending()
        self.assertEqual(len(rows), 2)
        self.assertNotEqual(rows[0]['id'], rows[1]['id'])

    async def test_outbox_delivers_original_target_without_a_connected_call(self):
        from sidevoice.connector_control import ConnectorControl
        from test_connector_control import FakePeer
        control = ConnectorControl(self.hub.journal, self.hub)
        self.addCleanup(lambda: [control.drop_inflight(b) for b in list(control.inflight)])
        peer = FakePeer()
        binding = self.hub.journal.register_binding('conn-1', harness='test', thread='a')
        control.peers['conn-1'] = peer; control.live[binding['id']] = 'conn-1'
        await self.hub.select(self.c.id, 'a')
        self.c.user_started(); self.c.enqueue_input('Para A')
        await self.hub.select(self.c.id, 'b')
        self.c.disconnect()
        await control.tick(); await peer.until_asked(1)
        self.assertEqual([event for event, _ in peer.asked], ['input.deliver'])
        self.assertEqual((peer.asked[0][1]['thread'], peer.asked[0][1]['text']), ('a', 'Para A'))
        self.assertEqual(self.hub.journal.history()[0]['status'], 'sending')
        await control.tick()
        self.assertEqual(len(peer.asked), 1)  # one delivery in flight per binding
        event_id = peer.asked[0][1]['event_id']
        await control.acknowledge('conn-2', event_id, {'status': 'accepted'})
        self.assertEqual(self.hub.journal.history()[0]['status'], 'sending')  # a stranger cannot settle it
        await control.acknowledge('conn-1', event_id, {'status': 'accepted'})
        self.assertEqual(self.hub.journal.history()[0]['status'], 'delivered')

    async def test_app_lifespan_starts_and_stops_durable_delivery(self):
        from contextlib import asynccontextmanager
        from fastapi import FastAPI
        from sidevoice.presentation import mount_presentation
        from sidevoice.connector_control import mount_connector_control
        events = []
        @asynccontextmanager
        async def existing_lifespan(app):
            events.append('start'); yield {'existing': True}; events.append('stop')
        app = FastAPI(lifespan=existing_lifespan)
        with patch('sidevoice.presentation.hub', self.hub):
            mount_presentation(app)
            control = mount_connector_control(app, self.hub)
            async with app.router.lifespan_context(app):
                self.assertIs(self.hub.control, control)
                self.assertIsNotNone(control.pump_task)
                self.assertFalse(control.pump_task.done())
            self.assertTrue(control.pump_task.cancelled() or control.pump_task.done())
        self.assertEqual(events, ['start', 'stop'])

    async def test_reply_waits_until_user_finishes_and_uses_application_playback_epoch(self):
        from sidevoice.presentation import Speech
        await self.hub.select(self.c.id, 'a')
        self.c.user_started(); self.c.speaking = False
        original = self.c.revision
        self.c.user_started()
        payload = Speech(thread_id='a',text='Respuesta anterior',session_id=self.c.id,
                         revision=original,utterance_id='waiting')
        result = await self.hub.publish(payload)
        self.assertEqual(result['status'], 'waiting_for_turn')
        row = self.hub.journal.history('a')[0]
        self.assertEqual(row['revision'], original)
        self.assertEqual(row['audio_reason'], 'user_speaking')
        self.c.worker.queue_frames.assert_not_awaited()
        self.c.user_started()  # Another speech start still cannot lose the waiting reply.
        self.c.speaking = False
        await self.c.dispatch()
        self.c.worker.queue_frames.assert_awaited_once()
        self.assertEqual(self.c.utterances['waiting'].revision, self.c.revision)

    async def test_waiting_reply_cut_by_a_focus_change_is_replayed_on_return(self):
        from sidevoice.presentation import Speech
        await self.hub.select(self.c.id, 'a')
        self.c.user_started()
        payload = Speech(thread_id='a',text='Pendiente',session_id=self.c.id,
                         revision=self.c.revision,utterance_id='waiting')
        await self.hub.publish(payload)
        await self.hub.select(self.c.id, 'b')
        self.assertEqual(self.hub.journal.history('a')[0]['audio_reason'], 'focus_changed')
        self.c.speaking = False
        await self.hub.select(self.c.id, 'a')
        # The original stays cut; what sounds is its replay, because the bubble promised it.
        result = await self.hub.publish(payload)
        self.assertEqual(result['status'], 'interrupted')
        self.c.worker.queue_frames.assert_awaited_once()
        self.assertEqual(self.c.worker.queue_frames.call_args.args[0][1].text, 'Pendiente')

    async def test_already_playing_interrupted_reply_is_never_replayed(self):
        from sidevoice.presentation import Speech
        await self.hub.select(self.c.id, 'a')
        payload = Speech(thread_id='a',text='Sonando',session_id=self.c.id,
                         revision=self.c.revision,utterance_id='playing')
        await self.hub.publish(payload)
        self.c.transition('playing', 'playing')
        self.c.user_started(); self.c.speaking = False
        await self.c.dispatch()
        result = await self.hub.publish(payload)
        self.assertEqual(result['status'], 'interrupted')
        self.c.worker.queue_frames.assert_awaited_once()

    async def test_browser_waiting_reply_dispatches_only_after_turn_end(self):
        from sidevoice.presentation import Speech
        await self.hub.select(self.c.id, 'a')
        events=[];self.c.on_browser_event=events.append
        self.c.user_started();events.clear()
        payload=Speech(thread_id='a',text='Espera',session_id=self.c.id,
                       revision=self.c.revision,utterance_id='browser-wait',language='es')
        await self.hub.publish(payload)
        self.assertEqual(events, [])
        self.c.speaking=False
        await self.c.dispatch()
        self.assertEqual(events[0]['type'], 'voice-speech')
        self.assertEqual(events[0]['data']['revision'], self.c.revision)
        self.assertEqual(self.hub.journal.history('a')[0]['status'], 'synthesizing')

    async def test_browser_preparing_audio_survives_another_user_turn(self):
        from sidevoice.presentation import Speech
        await self.hub.select(self.c.id, 'a')
        events=[];self.c.on_browser_event=events.append
        payload=Speech(thread_id='a',text='Todavía preparándose',session_id=self.c.id,
                       revision=self.c.revision,utterance_id='preparing')
        await self.hub.publish(payload)
        self.assertEqual(self.c.utterances['preparing'].status,'synthesizing')
        old_revision=self.c.revision
        self.c.user_started()
        self.assertEqual(self.c.utterances['preparing'].status,'waiting_for_turn')
        self.c.browser_cancelled('preparing',old_revision,False)
        self.c.speaking=False
        await self.c.dispatch()
        self.assertEqual(sum(e['type']=='voice-speech' for e in events),2)
        self.assertEqual(events[-1]['data']['revision'],self.c.revision)

    async def test_late_browser_cancellation_of_started_audio_prevents_replay(self):
        from sidevoice.presentation import Speech
        await self.hub.select(self.c.id, 'a')
        self.c.on_browser_event=lambda event:None
        payload=Speech(thread_id='a',text='Ya había sonado',session_id=self.c.id,
                       revision=self.c.revision,utterance_id='racing')
        await self.hub.publish(payload)
        old_revision=self.c.revision
        self.c.user_started()
        self.c.browser_cancelled('racing',old_revision,True)
        self.c.speaking=False
        await self.c.dispatch()
        self.assertEqual(self.c.utterances['racing'].status,'interrupted')
        self.assertIsNone(self.c.active)

    async def test_typed_message_is_literal_idempotent_and_does_not_replace_mic_turn(self):
        import json
        await self.hub.select(self.c.id, 'a')
        self.c.user_started()
        mic_revision=self.c.turn_revision
        args=('Texto escrito\ncon dos líneas',self.c.id,'a',self.c.target['binding_id'],'client-one')
        first=await self.hub.send_text(*args)
        second=await self.hub.send_text(*args)
        self.assertEqual(first,second)
        self.assertEqual(self.c.turn_revision,mic_revision)
        self.assertTrue(self.c.speaking)
        self.c.enqueue_input('Lo que estaba diciendo')
        rows=self.hub.journal.pending()
        self.assertEqual(len(rows),2)
        self.assertEqual(rows[0]['text'],'Texto escrito\ncon dos líneas')
        self.assertNotEqual(rows[0]['id'],rows[1]['id'])
        self.assertEqual(json.loads(rows[0]['payload'])['thread_id'],'a')

    async def test_text_rejects_changed_destination_and_conflicting_retry(self):
        await self.hub.select(self.c.id, 'a')
        original=self.c.target['binding_id']
        args=('Para A',self.c.id,'a',original,'one')
        await self.hub.send_text(*args)
        with self.assertRaises(HTTPException):
            await self.hub.send_text('Otro texto',self.c.id,'a',original,'one')
        await self.hub.select(self.c.id, 'b')
        with self.assertRaises(HTTPException):
            await self.hub.send_text('A antiguo',self.c.id,'a',original,'two')
        self.assertEqual(len(self.hub.journal.pending()),1)

    async def test_quiet_grace_waits_and_restarts_after_another_intervention(self):
        from sidevoice.presentation import Speech
        await self.hub.select(self.c.id, 'a')
        events=[];self.c.on_browser_event=events.append
        self.c.audio_grace_seconds=.08
        self.c.user_started();events.clear()
        payload=Speech(thread_id='a',text='Pendiente',session_id=self.c.id,
                       revision=self.c.revision,utterance_id='grace')
        await self.hub.publish(payload)
        await self.c.finish_user_turn()
        self.assertEqual(self.c.utterances['grace'].status,'waiting_for_pause')
        await asyncio.sleep(.025)
        self.assertFalse(any(e['type']=='voice-speech' for e in events))
        self.c.user_started()
        await asyncio.sleep(.09)
        self.assertFalse(any(e['type']=='voice-speech' for e in events))
        await self.c.finish_user_turn()
        await asyncio.sleep(.025)
        self.assertFalse(any(e['type']=='voice-speech' for e in events))
        await asyncio.sleep(.075)
        self.assertEqual(sum(e['type']=='voice-speech' for e in events),1)
        self.c.disconnect()

    async def test_close_channel_removes_the_binding_and_drops_waiting_input(self):
        from sidevoice.presentation import Speech
        record = self.hub.journal.register_binding('conn-1', harness='test', thread='a')
        await self.hub.select(self.c.id, 'a')
        self.c.user_started()
        self.c.enqueue_input('Todavía en cola')
        rev = self.c.revision
        first = await self.hub.close_channel('a')
        second = await self.hub.close_channel('a')
        self.assertEqual(first, {'status': 'closed', 'binding_id': record['id']})
        self.assertEqual(second, {'status': 'closed', 'binding_id': None})
        self.assertTrue(self.c.connected)
        self.assertIsNone(self.c.target['thread_id'])
        self.assertIsNone(self.hub.journal.binding_for_thread('a'))
        self.assertEqual(self.hub.journal.pending(), [])
        self.assertEqual(self.hub.journal.history('a')[-1]['status'], 'not_sent')
        # Nothing durable records the closure: a new room knows nothing about it.
        from pathlib import Path
        self.assertFalse(Path(self.hub.journal.state_path).exists())
        reply = await self.hub.publish(Speech(thread_id='a',session_id=self.c.id,revision=rev,text='Late',utterance_id='late'))
        self.assertEqual(reply['status'], 'text_only')
        await self.hub.select(self.c.id, 'a')
        self.assertEqual(self.c.target['thread_id'], 'a')

    async def test_cancelled_mic_turn_cannot_enter_outbox_but_next_turn_can(self):
        await self.hub.select(self.c.id, 'a')
        self.c.user_started()
        self.c.cancelled_turn = self.c.turn_revision
        self.c.enqueue_input('Do not send')
        self.assertEqual(self.hub.journal.pending(), [])
        self.c.user_started()
        self.c.enqueue_input('Send next')
        self.assertEqual(self.hub.journal.pending()[0]['text'], 'Send next')


class RoomSurfaceTests(IsolatedAsyncioTestCase):
    """What the room answers to, and what it refuses to describe."""

    def app(self):
        from sidevoice.app import create_app
        return create_app()

    async def test_the_root_leads_to_the_room_and_no_schema_is_published(self):
        from starlette.testclient import TestClient
        with TestClient(self.app()) as client:
            for path in ('/', '/voice'):
                answer = client.get(path, follow_redirects=False)
                self.assertIn(answer.status_code, (307, 308, 302))
                self.assertEqual(answer.headers['location'], '/voice/',
                                 'arriving at the room address is arriving at the room')
            for path in ('/docs', '/redoc', '/openapi.json'):
                self.assertEqual(client.get(path).status_code, 404,
                                 f'{path} would publish a map of every endpoint to anyone who asks')


class ClientErrorBeaconTests(IsolatedAsyncioTestCase):
    async def test_a_page_with_no_call_can_still_tell_the_room_why_it_went_blank(self):
        from fastapi import FastAPI
        from starlette.testclient import TestClient
        import sidevoice.presentation as presentation
        app = FastAPI()
        presentation.mount_presentation(app)
        before = len(presentation.hub.client_errors)
        with TestClient(app) as client:
            answer = client.post('/api/presentation/client-error',
                                 json={'kind': 'uncaught', 'message': 'name.trim is not a function',
                                       'stack': 'at initials', 'build': 'mu8y5e4k'},
                                 headers={'Origin': 'http://testserver'})
            self.assertEqual(answer.status_code, 200)
            self.assertEqual(answer.json(), {'status': 'recorded'})
        kept = list(presentation.hub.client_errors)[-1]
        self.assertEqual(len(presentation.hub.client_errors), before + 1)
        self.assertEqual((kept['kind'], kept['message'], kept['build']),
                         ('uncaught', 'name.trim is not a function', 'mu8y5e4k'))
        self.assertIsNone(kept['session_id'], 'a beacon comes from a page that has no call')


class CachePolicyTests(IsolatedAsyncioTestCase):
    async def test_the_audio_engine_and_page_shell_are_never_cached_while_hashed_assets_are_immutable(self):
        import tempfile
        from pathlib import Path
        from fastapi import FastAPI
        from starlette.testclient import TestClient
        from unittest.mock import patch
        import sidevoice.presentation as presentation
        with tempfile.TemporaryDirectory() as directory:
            browser, assets = Path(directory) / 'browser', Path(directory) / 'assets'
            browser.mkdir(); assets.mkdir()
            (browser / 'room-client.js').write_text('// engine')
            (assets / 'index-abc123.js').write_text('// bundle')
            with patch.object(presentation, 'BROWSER_AUDIO_DIST', browser), patch.object(presentation, 'WEB_DIST', Path(directory)):
                app = FastAPI()
                presentation.mount_presentation(app)
                with TestClient(app) as client:
                    engine = client.get('/voice-browser/room-client.js?v=abc')
                    self.assertEqual(engine.status_code, 200)
                    self.assertEqual(engine.headers['cache-control'], 'no-cache')
                    bundle = client.get('/voice/assets/index-abc123.js')
                    self.assertEqual(bundle.status_code, 200)
                    self.assertEqual(bundle.headers['cache-control'], 'public, max-age=31536000, immutable')


class ParticipantEngineTests(IsolatedAsyncioTestCase):
    """What a conversation thinks with travels to the page with the conversation, not beside it."""

    async def test_a_participant_says_what_it_thinks_with_and_observation_replaces_the_launch_line(self):
        from fastapi import FastAPI
        from starlette.testclient import TestClient
        import sidevoice.presentation as presentation
        app = FastAPI()
        presentation.mount_presentation(app)
        journal = presentation.hub.journal
        binding = journal.register_binding('c-1', harness='claude', thread='sess-engine', title='Trabajo',
                                           engine={'model': 'claude-opus-5'})
        self.addCleanup(journal.deactivate_binding, 'c-1', binding['id'])

        def row():
            answer = TestClient(app).get('/api/presentation/participants')
            self.assertEqual(answer.status_code, 200)
            return next(p for p in answer.json()['participants'] if p['thread_id'] == 'sess-engine')

        self.assertEqual(row()['engine'], {'model': 'claude-opus-5'},
                         'the launch line is what the row says until the harness has been observed')
        journal.set_binding_engine(binding['id'], {'model': 'claude-fable-5-1'})
        self.assertEqual(row()['engine'], {'model': 'claude-fable-5-1'}, 'what was observed replaces it')
        # A reconnection re-registers the binding; it does not take the observation away.
        journal.register_binding('c-1', harness='claude', thread='sess-engine', title='Trabajo', binding_id=binding['id'])
        self.assertEqual(row()['engine'], {'model': 'claude-fable-5-1'})
        # A conversation whose harness never said is a row all the same, saying nothing about a model.
        silent = journal.register_binding('c-1', harness='codex', thread='sess-silent', title='Otra')
        self.addCleanup(journal.deactivate_binding, 'c-1', silent['id'])
        answer = TestClient(app).get('/api/presentation/participants').json()['participants']
        self.assertIsNone(next(p for p in answer if p['thread_id'] == 'sess-silent')['engine'])


class AdmissionEndpointTests(IsolatedAsyncioTestCase):
    """The third way a refusal reaches the person, and the only one a proxy cannot spoil (#63)."""

    async def test_the_room_says_over_http_whether_it_would_take_one_more_browser(self):
        from fastapi import FastAPI
        from starlette.testclient import TestClient
        import sidevoice.presentation as presentation
        app = FastAPI()
        presentation.mount_presentation(app)
        answer = TestClient(app).get('/api/presentation/admission')
        self.assertEqual(answer.status_code, 200)
        self.assertEqual(answer.json(), presentation.hub.admission())
        self.assertTrue(answer.json()['admitted'], 'an empty room takes one more')
        # The page reads this exactly when its socket closed saying nothing: the answer has to name
        # the reason, not only describe the room.
        with patch.object(presentation.hub, 'max_clients', 0):
            refused = TestClient(app).get('/api/presentation/admission').json()
        self.assertEqual((refused['admitted'], refused['reason']), (False, 'room_is_full'))
        self.assertEqual(refused['message'], presentation.hub.FULL_MESSAGE)
