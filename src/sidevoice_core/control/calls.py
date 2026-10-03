"""One call, from the client's hello to its last frame: a room listener or an echo test gets a pipeline.

This is where the control plane creates the pipeline it owns one of per call. What carries the
audio is handed in already built — a Pipecat transport and the serializer that frames it — so
nothing here knows whether a WebSocket, a relayed socket or a peer connection is underneath.
"""
import asyncio
import time
import uuid

from loguru import logger
from pipecat.frames.frames import OutputTransportMessageUrgentFrame

from ..pipeline import transcription
from ..pipeline.call import CallPipeline, VoiceCall, browser_runtime, prior_sessions
from ..pipeline.heartbeat import heartbeat_settings, watch as watch_heartbeat
from ..pipeline.serializer import session_message
from .room import RoomClient


async def run_call(room, transport, serializer, *, settings, config, choice, hello, refuse, settings_problem=None,
                   echo=False, close_socket=None):
    """One pipeline for every call: PCM in, the device's turn detection, and a transcription provider.

    The provider is the client itself or a provider called from here; the pipeline never knows which.

    A device that changes a setting this pipeline was built from opens a second socket instead of
    hanging up, so the same browser may hold two of these at once for as long as the swap takes.
    Nothing here is shared between them: each has its own client id, its own selection and its own
    epoch, and the one being replaced leaves without touching the one that replaced it.

    `refuse(message)` tells the client why it cannot join and closes its connection.
    """
    from ..pipeline.settings import mic_settings
    mic, problem = mic_settings(settings, hello.get('mic'))
    problem = settings_problem or problem
    outbox = asyncio.Queue()
    send = outbox.put_nowait

    async def deliver():
        while True:
            message = await outbox.get()
            try:
                await transport.output().send_message(OutputTransportMessageUrgentFrame(message=message))
            finally:
                outbox.task_done()

    if echo:
        from .echo import EchoCall
        call = EchoCall(str(uuid.uuid4()), settings=settings, config=config, send=send)
        returning = []
    else:
        try:
            call = RoomClient(str(uuid.uuid4()), room)
        except RuntimeError as error:
            # The room filled up between the check before the hello and this join. A browser changing a
            # setting that needs another pipeline holds two sockets for a moment, so the race is real:
            # refusing the newcomer is the whole point, and it disturbs nobody already in the room.
            await refuse(str(error))
            return
        returning = prior_sessions(hello.get('sessions'))
        wanted = hello.get('conversation')
        if isinstance(wanted, str) and wanted:
            # The browser names the conversation it was talking to (its own state, kept across a reload);
            # it is honoured only if that conversation is still connected to the room.
            record = room.journal.binding_for_thread(wanted) if room.journal else None
            if record:
                call.target = {'thread_id': wanted, 'title': record.get('title'), 'binding_id': str(uuid.uuid4())}
    provider = transcription.build(choice, config=config, send=send, session_id=call.id)
    pipeline = CallPipeline(transport, mic=mic, config=config, provider=provider, language=choice['language'])
    transcriber = pipeline.transcriber
    runtime, runtime_problem = None, None
    try:
        runtime = browser_runtime(hello.get('transcription'))
    except ValueError as error:
        runtime_problem = str(error)
    if runtime and runtime.get('fallback_error'):
        # The client offered an accelerator and could not load the model on it: said here as well as in the stats.
        logger.warning('Call {}: local transcription fell back from {} to {}: {}', call.id[:8], runtime['fallback_from'],
                       runtime['accelerator'], runtime['fallback_error'])
    voice = VoiceCall(call, transcriber, send, settings=settings, mic=mic, choice=choice, runtime=runtime, config=config,
                      vad_stop_secs=pipeline.vad_stop_secs, vad=pipeline.vad)
    # The hello carries the browser's call span, so the room's turns are inside the browser's call
    # and not a trace of their own. What this call is made of goes on it once, never on every turn.
    told = hello.get('telemetry') if isinstance(hello.get('telemetry'), dict) else {}
    call.telemetry.call_started(told.get('traceparent'), {
        'sidevoice.stt_place': choice['place'], 'sidevoice.stt_model': call.transcription.get('model'),
        'sidevoice.stt_accelerator': call.transcription.get('accelerator'), 'sidevoice.turn_end_mode': mic.turn_end_mode})
    call.mic = serializer
    problems = [message for message in (problem, runtime_problem) if message]
    logger.info('Call {}: transcription {} · {} · {}, turn end {}', call.id[:8], choice['place'],
                call.transcription.get('model'), call.transcription.get('accelerator') or '-', mic.turn_end_mode)

    await pipeline.serve(call, voice)
    call.feed_audio = pipeline.feed   # a second path for the microphone, when the client negotiates one
    sender = asyncio.create_task(deliver())

    deadline_task = None

    async def expire_echo():
        from .echo import MAX_SECONDS
        await asyncio.sleep(MAX_SECONDS)
        if not call.connected:
            return
        error = call.deadline_error(getattr(serializer, 'audio_frames', 0))
        send({'type': 'echo.error', 'data': error})
        send({'type': 'echo.session-ended', 'data': {'session_id': call.id, 'reason': 'deadline'}})
        await outbox.join()
        if close_socket:
            try:
                await close_socket()
            except RuntimeError:
                pass
        await pipeline.cancel()

    @transport.event_handler('on_client_connected')
    async def connected(transport, client):
        nonlocal deadline_task
        call.connected = True
        await transport.output().send_message(
            OutputTransportMessageUrgentFrame(message=session_message(call.id, serializer)))
        if echo:
            from ..i18n import keyed_message
            from .echo import MAX_SECONDS
            await transport.output().send_message(OutputTransportMessageUrgentFrame(message={
                'type': 'echo.session', 'data': {'session_id': call.id, 'max_duration_seconds': MAX_SECONDS}}))
            if settings_problem:
                send({'type': 'echo.error', 'data': keyed_message('echo.settings-invalid', settings.ui_language)})
            if problem and not settings_problem:
                send({'type': 'echo.error', 'data': keyed_message('echo.settings-invalid', settings.ui_language)})
            if runtime_problem:
                send({'type': 'echo.error', 'data': keyed_message('echo.transcription-runtime-invalid', settings.ui_language,
                                                                  stage='transcription')})
            deadline_task = asyncio.create_task(expire_echo())
        else:
            # Only now can anything reach the browser: what its hello got wrong goes right after the session.
            for message in problems:
                send({'type': 'error', 'data': {'message': message}})
            call.room.report_conversation_working(call)
            # A person coming back from a tunnel cannot read the transcript. What this browser never heard
            # through goes to it now, oldest first and ahead of anything new, for as long back as this
            # device asked for. Nothing is stored for it: the room already had every one of them.
            caught_up = await call.room.replay(call, seconds=settings.replay_on_return_seconds,
                                               sessions=returning)
            if caught_up['replayed'] or caught_up['skipped']:
                logger.info('Call {}: replaying {} replies this browser never heard, {} without audio',
                            call.id[:8], len(caught_up['replayed']), len(caught_up['skipped']))

    @transport.event_handler('on_client_disconnected')
    async def disconnected(transport, client):
        call.disconnect()
        await pipeline.cancel()

    # A browser that stopped answering leaves by the door above, and this is what knocks on it: a
    # socket nobody is at the other end of never closes by itself behind a proxy, so the room asks,
    # and a browser that has missed its budget of answers is disconnected exactly as if its socket
    # had closed. Nothing downstream is told it was a timeout, because nothing downstream differs.
    interval, misses = heartbeat_settings(config)

    def ask():
        send({'type': 'voice-ping', 'data': {'session_id': call.id}})

    async def drop(silence):
        if echo:
            logger.warning('Call {}: nothing from this browser for {:.0f}s; ending its socket', call.id[:8], silence)
        else:
            logger.warning('Call {}: nothing from this browser for {:.0f}s; its seat goes back to the room',
                           call.id[:8], silence)
        call.disconnect()
        await pipeline.cancel()

    heartbeat = asyncio.create_task(watch_heartbeat(
        lambda: time.monotonic() - serializer.last_frame_at, ask, drop,
        interval=interval, misses=misses)) if interval else None

    try:
        await pipeline.run()
    finally:
        if deadline_task:
            deadline_task.cancel()
        if heartbeat:
            heartbeat.cancel()
        voice.close()
        call.disconnect()
        sender.cancel()
        if call.media_peer is not None:
            # Whatever else carried this call's microphone ends with the call.
            await call.media_peer.close()
