"""This node's side of the rendezvous: one link with the hosted room, and the relay it carries.

The room no longer holds conversations; it joins a browser with the node that does. The link between
the two is Socket.IO and either side may open it (rubasace/sidevoice `docs/RENDEZVOUS.md`):

- **outbound**, the normal case — this machine is usually not reachable (a laptop, a pod): the core
  dials the room with the machine's pairing credential, the file `sidevoice pair` wrote;
- **dial** — a reachable node: the room connects to `/api/rendezvous/link` here, proves it is this
  machine's room with the dial key it handed out at pairing, and this node proves itself back with its
  token (`node.hello`).

Whichever side opened it, the room then asks the same things: `relay.http` (a browser's request) and
`relay.open` / `relay.data` / `relay.close` (a browser's call socket). `Relay` serves them by making the
very same request to this node on loopback: the relay adds no second implementation of any endpoint,
and a relayed browser is a browser like any other to everything behind it.
"""
import asyncio
import json
import secrets
import socket
from pathlib import Path
from urllib.parse import urlsplit

from loguru import logger

PROTOCOL = 3
PATH = '/api/connectors/link'      # on the room: the path its proxy already exempts
NAMESPACE = '/nodes'
DIAL_PATH = '/api/rendezvous/link'  # on this node, for a room that dials it
DIAL_NAMESPACE = '/room'
RELAYED = '/api/presentation'


def private_network(hostname):
    """Where a credential may travel in clear: loopback and a Kubernetes service name, the same rule the
    connector applies (`pair.mjs`). Anything else needs TLS."""
    import re
    if hostname in {'127.0.0.1', 'localhost', '::1', '[::1]'}:
        return True
    return bool(re.fullmatch(r'[a-z0-9-]+\.[a-z0-9-]+\.svc(\.[a-z0-9.-]+)?', hostname or '', re.I))


def read_pairing(path):
    """The machine's pairing, as `sidevoice pair` wrote it, or None. This node only reads it."""
    try:
        saved = json.loads(Path(path).read_text(encoding='utf8'))
    except (OSError, ValueError, TypeError):
        return None
    if not isinstance(saved, dict) or not all(isinstance(saved.get(key), str) and saved[key]
                                              for key in ('url', 'connector_id', 'token')):
        return None
    parts = urlsplit(saved['url'])
    origin = f'{"https" if parts.scheme == "wss" else "http" if parts.scheme == "ws" else parts.scheme}://{parts.netloc}'
    return {**saved, 'origin': origin}


class Relay:
    """The room's requests, made to this node on loopback. One per link; its channels end with it."""

    def __init__(self, base_url, emit):
        self.base = base_url.rstrip('/')
        self.emit = emit          # async (event, data): say something to the room over this link
        self.channels = {}        # channel -> (aiohttp websocket, pump task)
        self.http_session = None

    def session(self):
        import aiohttp
        if self.http_session is None or self.http_session.closed:
            self.http_session = aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=60))
        return self.http_session

    def headers(self, told):
        told = told if isinstance(told, dict) else {}
        kept = {name: str(told[name]) for name in ('content-type', 'accept') if told.get(name)}
        # The room checked the browser's origin against its own before relaying. Here the request comes
        # from this node's own origin, which is what it now is; an endpoint that insists on an Origin
        # (the credential forms) still sees one only when the browser sent one.
        if told.get('origin'):
            kept['origin'] = self.base
        return kept

    async def http(self, data):
        data = data if isinstance(data, dict) else {}
        path, method = str(data.get('path') or ''), str(data.get('method') or 'GET').upper()
        if not path.startswith(RELAYED) or '..' in path or method not in {'GET', 'POST', 'PUT', 'DELETE', 'PATCH'}:
            return {'status': 404, 'headers': {'content-type': 'application/json'},
                    'body': json.dumps({'detail': 'Not relayed.'}).encode()}
        query = data.get('query') if isinstance(data.get('query'), str) else ''
        try:
            async with self.session().request(method, self.base + path + ('?' + query if query else ''),
                                              data=data.get('body') or None, headers=self.headers(data.get('headers'))) as answer:
                return {'status': answer.status, 'headers': {'content-type': answer.headers.get('content-type', '')},
                        'body': await answer.read()}
        except Exception as error:
            return {'status': 502, 'headers': {'content-type': 'application/json'},
                    'body': json.dumps({'detail': 'The node could not answer: ' + type(error).__name__}).encode()}

    async def open(self, data):
        import aiohttp
        data = data if isinstance(data, dict) else {}
        channel, path = data.get('channel'), str(data.get('path') or '')
        if not isinstance(channel, str) or not channel or path != RELAYED + '/ws' or channel in self.channels:
            return {'ok': False, 'status': 404, 'detail': 'Not relayed.'}
        query = data.get('query') if isinstance(data.get('query'), str) else ''
        url = 'ws' + self.base[4:] + path + ('?' + query if query else '')
        try:
            ws = await self.session().ws_connect(url, headers={'origin': self.base}, max_msg_size=0, autoping=True)
        except aiohttp.WSServerHandshakeError as error:
            return {'ok': False, 'status': error.status, 'detail': error.message}
        except Exception as error:
            return {'ok': False, 'status': 502, 'detail': type(error).__name__}
        self.channels[channel] = (ws, asyncio.create_task(self.pump(channel, ws)))
        return {'ok': True}

    async def pump(self, channel, ws):
        """What the node says on a relayed call socket goes to the room, in order, until either side closes."""
        import aiohttp
        try:
            async for message in ws:
                if message.type == aiohttp.WSMsgType.TEXT:
                    await self.emit('relay.data', {'channel': channel, 'data': message.data})
                elif message.type == aiohttp.WSMsgType.BINARY:
                    await self.emit('relay.data', {'channel': channel, 'data': bytes(message.data)})
                else:
                    break
        except asyncio.CancelledError:
            raise
        except Exception as error:
            logger.info('Relayed channel {} ended: {}', channel[:8], type(error).__name__)
        finally:
            if self.channels.pop(channel, None) is not None:
                try:
                    await self.emit('relay.close', {'channel': channel, 'code': ws.close_code or 1000, 'reason': ''})
                except Exception:
                    pass

    async def data(self, data):
        data = data if isinstance(data, dict) else {}
        entry = self.channels.get(data.get('channel'))
        if entry is None:
            return
        payload = data.get('data')
        try:
            if isinstance(payload, (bytes, bytearray)):
                await entry[0].send_bytes(bytes(payload))
            elif isinstance(payload, str):
                await entry[0].send_str(payload)
        except Exception:
            pass

    async def close(self, data):
        data = data if isinstance(data, dict) else {}
        entry = self.channels.pop(data.get('channel'), None)
        if entry is None:
            return
        ws, pump = entry
        code = data.get('code') if isinstance(data.get('code'), int) and 1000 <= data['code'] < 5000 else 1000
        try:
            await ws.close(code=code)
        except Exception:
            pass
        pump.cancel()

    async def shutdown(self):
        """The link is gone: every relayed call ends, as a browser whose socket dropped. It comes back."""
        for channel in list(self.channels):
            ws, pump = self.channels.pop(channel)
            pump.cancel()
            try:
                await ws.close()
            except Exception:
                pass
        if self.http_session is not None and not self.http_session.closed:
            await self.http_session.close()


def identity(control):
    """What this machine says about itself to the room: what its connector last said to this core, and
    the core's own version. A core no connector has linked to yet says only its host name."""
    from ..runtime import version
    told = dict(getattr(control, 'identity', None) or {})
    told.setdefault('host', socket.gethostname())
    return {**told, 'core': version()}


class Rendezvous:
    """The link with the room, whoever opened it, and what this node reports about it to its connector.

    `state` is `{room, connected, via, error, refused}`; `on_state(state)` is told every change.
    """

    RETRY_FIRST, RETRY_MAX = 0.25, 10.0

    def __init__(self, pairing_path, base_url, *, control=None, on_state=None, poll=2.0):
        self.pairing_path = Path(pairing_path) if pairing_path else None
        self.base = base_url
        self.control = control
        self.on_state = on_state
        self.poll = poll
        self.state = {'room': None, 'connected': False, 'via': None, 'error': None, 'refused': None}
        self.client = None
        self.relay = None
        self.task = None
        self.pairing = None

    # ----- what the connector hears -----

    async def report(self, **changes):
        state = {**self.state, **changes}
        if state == self.state:
            return
        self.state = state
        if self.on_state:
            try:
                await self.on_state(dict(state))
            except Exception as error:
                logger.warning('Could not report the rendezvous state: {}', error)

    # ----- outbound -----

    async def start(self):
        if self.pairing_path is not None:
            self.task = asyncio.create_task(self.run())

    async def stop(self):
        if self.task:
            self.task.cancel()
            await asyncio.gather(self.task, return_exceptions=True)
        await self.hang_up()

    async def hang_up(self):
        client, self.client = self.client, None
        if client is not None:
            try:
                await client.disconnect()
            except Exception:
                pass
        if self.relay is not None:
            await self.relay.shutdown()
            self.relay = None

    async def run(self):
        """Keep one link to the paired room while there is a pairing; follow the file when it changes."""
        delay, seen = self.RETRY_FIRST, object()   # nothing seen yet: the first look always reports
        while True:
            pairing = read_pairing(self.pairing_path)
            key = json.dumps(pairing, sort_keys=True) if pairing else None
            if key != seen:
                # Paired, re-paired or unpaired since last look: whatever link there was belongs to the old one.
                seen, delay = key, self.RETRY_FIRST
                await self.hang_up()
                self.pairing = pairing
                await self.report(room=pairing and pairing['origin'], connected=False, via=None, error=None, refused=None)
            if pairing and self.client is None and not self.state['refused']:
                if await self.dial_room(pairing):
                    delay = self.RETRY_FIRST
                else:
                    await asyncio.sleep(delay)
                    delay = min(delay * 2, self.RETRY_MAX)
                    continue
            await asyncio.sleep(self.poll)

    async def dial_room(self, pairing):
        import socketio
        origin = pairing['origin']
        if urlsplit(origin).scheme != 'https' and not private_network(urlsplit(origin).hostname):
            await self.report(error='The room URL must be https:// unless it is on this machine or inside its cluster.')
            return False
        client = socketio.AsyncClient(reconnection=True, reconnection_delay=self.RETRY_FIRST,
                                      reconnection_delay_max=self.RETRY_MAX, logger=False, engineio_logger=False)
        refused = {}
        relay = Relay(self.base, lambda event, data: client.emit(event, data, namespace=NAMESPACE))

        @client.on('connect_error', namespace=NAMESPACE)
        async def connect_error(data):
            refused['reason'] = data.get('message') if isinstance(data, dict) else str(data or '')

        @client.on('node.welcome', namespace=NAMESPACE)
        async def welcome(data):
            await self.report(connected=True, via='outbound', error=None, refused=None)
            logger.info('Linked with the room at {} (outbound)', origin)

        @client.on('disconnect', namespace=NAMESPACE)
        async def disconnect(*_):
            await relay.shutdown()
            if self.client is client:
                await self.report(connected=False, error='The link with the room dropped; it comes back on its own.')

        @client.on('node.revoked', namespace=NAMESPACE)
        async def revoked(data):
            reason = (data or {}).get('reason') or 'The room revoked this machine\'s pairing.'
            logger.warning('The room revoked this machine: {}', reason)
            await self.report(connected=False, refused=reason)
            asyncio.create_task(self.hang_up())

        self.serve(client.on, lambda: relay, namespace=NAMESPACE)
        auth = {'connector_id': pairing['connector_id'], 'token': pairing['token'], 'protocol': PROTOCOL,
                **identity(self.control)}
        try:
            await client.connect(origin, namespaces=[NAMESPACE], transports=['websocket'], socketio_path=PATH,
                                 auth=auth, wait_timeout=10)
        except Exception as error:
            await relay.shutdown()
            reason = refused.get('reason')
            if reason and reason not in {'One or more namespaces failed to connect'}:
                # The room's own words: it will not have this machine, and asking again will not change that.
                await self.report(connected=False, refused=reason, error=None)
            else:
                await self.report(connected=False, error=f'The room is unreachable ({type(error).__name__}); retrying.')
            return False
        self.client, self.relay = client, relay
        return True

    @staticmethod
    def serve(on, relay_for, **where):
        """The relay's four events on a link, whichever library object carries it. A client's handlers get
        the data; a server's get the connection's id first: the data is always the last argument."""
        for event in ('relay.http', 'relay.open', 'relay.data', 'relay.close'):
            async def handler(*args, method=event.split('.', 1)[1]):
                relay = relay_for(*args[:-1])
                return await getattr(relay, method)(args[-1]) if relay is not None else None
            on(event, **where)(handler)

    # ----- a room that dials this node -----

    def mount_dial(self, app):
        """Accept the paired room on `/api/rendezvous/link`. Only a node someone made reachable is ever
        dialled; for everybody else this route answers nothing but refusals."""
        import socketio
        from socketio.exceptions import ConnectionRefusedError
        server = socketio.AsyncServer(async_mode='asgi', namespaces=[DIAL_NAMESPACE], cors_allowed_origins=[])
        relays = {}

        @server.event(namespace=DIAL_NAMESPACE)
        async def connect(sid, environ, auth):
            pairing = read_pairing(self.pairing_path) if self.pairing_path else None
            told = auth if isinstance(auth, dict) else {}
            if (not pairing or not pairing.get('dial_key') or told.get('connector_id') != pairing['connector_id']
                    or not secrets.compare_digest(str(told.get('dial_key') or ''), pairing['dial_key'])):
                raise ConnectionRefusedError('This node is not paired with the room that is dialling it.')
            relay = Relay(self.base, lambda event, data: server.emit(event, data, to=sid, namespace=DIAL_NAMESPACE))
            relays[sid] = relay
            asyncio.create_task(self.greet(server, sid, pairing))

        @server.event(namespace=DIAL_NAMESPACE)
        async def disconnect(sid, reason=None):
            relay = relays.pop(sid, None)
            if relay is not None:
                await relay.shutdown()
            if not relays and self.state['via'] == 'dial':
                await self.report(connected=False, via=None)

        self.serve(server.on, relays.get, namespace=DIAL_NAMESPACE)

        @server.on('node.revoked', namespace=DIAL_NAMESPACE)
        async def revoked(sid, data):
            await self.report(connected=False, refused=(data or {}).get('reason') or 'The room revoked this machine\'s pairing.')
            await server.disconnect(sid, namespace=DIAL_NAMESPACE)

        app.mount(DIAL_PATH, socketio.ASGIApp(server, socketio_path=''))
        return server

    async def greet(self, server, sid, pairing):
        """The room proved it is this machine's; now this node proves it is the machine the room paired."""
        try:
            answer = await server.call('node.hello', {'connector_id': pairing['connector_id'], 'token': pairing['token'],
                                                      'protocol': PROTOCOL, **identity(self.control)},
                                       to=sid, namespace=DIAL_NAMESPACE, timeout=10)
        except Exception as error:
            logger.warning('A dialling room never answered this node\'s hello: {}', type(error).__name__)
            await server.disconnect(sid, namespace=DIAL_NAMESPACE)
            return
        if not isinstance(answer, dict) or answer.get('error'):
            reason = (answer or {}).get('error') if isinstance(answer, dict) else 'refused'
            await self.report(connected=False, refused=reason)
            await server.disconnect(sid, namespace=DIAL_NAMESPACE)
            return
        await self.report(room=pairing['origin'], connected=True, via='dial', error=None, refused=None)
        logger.info('Linked with the room at {} (it dialled this node)', pairing['origin'])
