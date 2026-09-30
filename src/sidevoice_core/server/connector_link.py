"""The connector link: Socket.IO at `/api/connectors/link`, namespace `/connectors`.

One `AsyncServer` mounted on the room's own FastAPI app, translating events into
`ConnectorControl` calls. Everything this module does not contain is the point of it:
acknowledgements, keepalive, reconnection with backoff and multiplexing are the library's,
and the room no longer maintains a protocol to get them.

The path is not the library's default `/socket.io` because it is what a proxy in front of the
room exempts from its login, exactly and by name; the namespace keeps the browser's future link
apart from this one on the same server.
"""
import socketio
from loguru import logger
# python-socketio's own ConnectionRefusedError, never the builtin of that name: the builtin is
# caught by a different branch that throws the message away, and the message is what tells the
# person at the other end to pair again.
from socketio.exceptions import ConnectionRefusedError, TimeoutError as AcknowledgementTimeout

from .connector_control import REVOKED_REASON, ConnectorPeer, HEARTBEAT_MISSES, PROTOCOL

# The route names the capability, not the transport: a machine reaches the room's connector link
# here whatever carries it, so a change of transport moves no Ingress and no credential.
PATH = '/api/connectors/link'
NAMESPACE = '/connectors'


class SocketIOPeer(ConnectorPeer):
    """One connector's socket, as the control plane sees it."""

    def __init__(self, server, sid):
        self.server, self.sid = server, sid

    async def send(self, event, data):
        await self.server.emit(event, data, to=self.sid, namespace=NAMESPACE)

    async def request(self, event, data, *, timeout):
        """The library's acknowledgement, on the room's clock. An answer that never came is a
        `TimeoutError` like any other wait, because the control plane treats it as one and must
        not have to know what carried it."""
        try:
            return await self.server.call(event, data, to=self.sid, namespace=NAMESPACE, timeout=timeout)
        except AcknowledgementTimeout as error:
            raise TimeoutError(f'{event} went unacknowledged for {timeout:g}s') from error

    async def disconnect(self):
        try:
            await self.server.disconnect(self.sid, namespace=NAMESPACE)
        except Exception:
            pass


def mount_connector_socketio(app, control):
    """Serve the link on `app`, driving `control`. Returns the server, for the tests that drive it."""
    server = socketio.AsyncServer(
        async_mode='asgi', namespaces=[NAMESPACE], cors_allowed_origins=[],
        # The keepalive budget the room has always allowed, now said in the library's words: it
        # asks this often, and gives up when that many are unanswered.
        ping_interval=control.heartbeat_seconds,
        ping_timeout=control.heartbeat_seconds * HEARTBEAT_MISSES)

    async def session(sid):
        try:
            return await server.get_session(sid, namespace=NAMESPACE)
        except KeyError:
            return {}

    async def speaker(sid):
        """Which connector this socket speaks for, or None if it never said. Every control method
        checks the answer against the binding, so None settles nothing and owns nothing."""
        return (await session(sid)).get('connector_id')

    @server.event(namespace=NAMESPACE)
    async def connect(sid, environ, auth):
        credential = auth if isinstance(auth, dict) else {}
        connector_id, token = credential.get('connector_id'), credential.get('token')
        # A machine says who it is on every connection, not only when it was paired: the room keeps
        # the latest, so the page shows what is true now rather than what was true months ago.
        if not control.journal.authenticate_connector(connector_id, token, credential):
            # A pairing the person took away is not a credential nobody recognises, and saying so is
            # the difference between "pair this machine again" and "somebody revoked it on purpose".
            raise ConnectionRefusedError(
                REVOKED_REASON if control.journal.connector_credential(connector_id, token) == 'revoked' else
                'This machine is not paired with the room, or its credential is no longer valid: '
                'pair it again with the code the room shows under "Emparejar máquina" (Pair a machine).')
        if credential.get('protocol') != PROTOCOL:
            raise ConnectionRefusedError(
                f'This connector speaks protocol {credential.get("protocol")!r} and the room speaks '
                f'{PROTOCOL}: update the connector and pair this machine again.')
        peer = SocketIOPeer(server, sid)
        await server.save_session(sid, {'connector_id': connector_id, 'peer': peer}, namespace=NAMESPACE)
        await control.attach(connector_id, peer)
        await server.emit('connector.welcome', {'protocol': PROTOCOL}, to=sid, namespace=NAMESPACE)
        logger.info('Connector {} connected from {} (version {})', connector_id,
                    credential.get('host') or 'an unnamed host', credential.get('version') or 'unknown')

    @server.event(namespace=NAMESPACE)
    async def disconnect(sid, reason=None):
        known = await session(sid)
        control.detach(known.get('connector_id'), known.get('peer'))

    @server.on('binding.register', namespace=NAMESPACE)
    async def binding_register(sid, data):
        """The one event whose acknowledgement can be a refusal: the connector must be able to tell
        a binding the room will never accept from one it has not answered yet."""
        try:
            return await control.register(await speaker(sid), data or {})
        except ValueError as error:
            return {'error': str(error)}

    @server.on('binding.unregister', namespace=NAMESPACE)
    async def binding_unregister(sid, data):
        await control.unregister(await speaker(sid), data or {})

    @server.on('speech.publish', namespace=NAMESPACE)
    async def speech_publish(sid, data):
        return await control.speech(await speaker(sid), data or {})

    @server.on('input.working', namespace=NAMESPACE)
    async def input_working(sid, data):
        await control.working(await speaker(sid), data or {})

    @server.on('input.engine', namespace=NAMESPACE)
    async def input_engine(sid, data):
        await control.engine(await speaker(sid), data or {})

    @server.on('input.read', namespace=NAMESPACE)
    async def input_read(sid, data):
        await control.read(await speaker(sid), data or {})

    app.mount(PATH, socketio.ASGIApp(server, socketio_path=''))
    return server
