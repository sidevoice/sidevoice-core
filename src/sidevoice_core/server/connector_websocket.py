"""Protocol 3's bounded symmetric RPC adapter over the node's existing local socket."""
import asyncio
import json

from fastapi import WebSocket, WebSocketDisconnect
from loguru import logger

from ..control.connectors import InvalidConnectorAcknowledgement, validate_delivery_acknowledgement

PATH = '/api/connectors/v3'
CONNECTOR_PROTOCOL = 3
HELLO_TIMEOUT_SECONDS = 10.0
HANDLER_TIMEOUT_SECONDS = 60.0
MAX_FRAME_BYTES = 1024 * 1024
MAX_PENDING_REQUESTS = 128
MAX_PENDING_HANDLERS = 32
MAX_RPC_ID_LENGTH = 100
NO_REMOTE_ERROR = object()


class WireError(Exception):
    pass


class FrameTooLarge(WireError):
    pass


class RemoteRPCError(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def _id_valid(value):
    return ((isinstance(value, str) and 0 < len(value) <= MAX_RPC_ID_LENGTH)
            or (type(value) is int and -(2 ** 53 - 1) <= value <= 2 ** 53 - 1))


def _encode(message):
    try:
        encoded = json.dumps(message, ensure_ascii=False, allow_nan=False,
                             separators=(',', ':')).encode('utf8')
    except (TypeError, ValueError, UnicodeError) as error:
        raise WireError('The message is not JSON serializable') from error
    if len(encoded) > MAX_FRAME_BYTES:
        raise WireError('The message exceeds the frame limit')
    return encoded.decode('utf8')


def _reject_json_constant(value):
    raise ValueError(f'Invalid JSON constant: {value}')


def _decode(text):
    if not isinstance(text, str):
        raise WireError('The frame is not text')
    if len(text.encode('utf8')) > MAX_FRAME_BYTES:
        raise FrameTooLarge('The frame exceeds the limit')
    try:
        frame = json.loads(text, parse_constant=_reject_json_constant)
    except (ValueError, UnicodeError) as error:
        raise WireError('The frame is not valid JSON') from error
    if not isinstance(frame, dict) or frame.get('jsonrpc') != '2.0':
        raise WireError('Expected one JSON-RPC 2.0 object')
    if 'method' in frame:
        method = frame.get('method')
        if (not isinstance(method, str) or not method or len(method) > 128
                or 'result' in frame or 'error' in frame):
            raise WireError('Invalid JSON-RPC request')
        if 'id' in frame and not _id_valid(frame['id']):
            raise WireError('Invalid JSON-RPC request id')
        params = frame.get('params', {})
        if not isinstance(params, dict):
            raise WireError('JSON-RPC params must be an object')
        return 'request', frame.get('id'), method, params
    if 'id' not in frame or not _id_valid(frame['id']):
        raise WireError('Invalid JSON-RPC response')
    if ('result' in frame) == ('error' in frame):
        raise WireError('A JSON-RPC response must contain result or error')
    return ('response', frame['id'], frame.get('result'),
            frame['error'] if 'error' in frame else NO_REMOTE_ERROR)


def _error(request_id, code, message):
    return {'jsonrpc': '2.0', 'id': request_id, 'error': {'code': code, 'message': message[:256]}}


class WebSocketPeer:
    """A ConnectorPeer with one reader, correlated requests in both directions, and bounded work."""

    def __init__(self, websocket):
        self.websocket = websocket
        self.closed = False
        self.sequence = 0
        self.pending = {}       # core request id -> (event, Future)
        self.handlers = set()   # connector request handlers
        self.incoming_ids = set()
        self.write_lock = asyncio.Lock()

    async def _write(self, frame):
        text = _encode(frame)
        async with self.write_lock:
            if self.closed:
                raise ConnectionError('Connector WebSocket is closed')
            await self.websocket.send_text(text)

    async def send(self, event, data):
        await self._write({'jsonrpc': '2.0', 'method': event, 'params': data})

    async def request(self, event, data, *, timeout):
        if self.closed:
            raise ConnectionError('Connector WebSocket is closed')
        if len(self.pending) >= MAX_PENDING_REQUESTS:
            raise RuntimeError('Too many pending connector requests')
        self.sequence += 1
        request_id = f's:{self.sequence}'
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = (event, future)
        deadline = asyncio.get_running_loop().time() + timeout
        try:
            try:
                async with asyncio.timeout_at(deadline):
                    await self._write({'jsonrpc': '2.0', 'id': request_id, 'method': event, 'params': data})
                    result = await future
            except TimeoutError as error:
                raise TimeoutError(f'{event} went unacknowledged for {timeout:g}s') from error
            if event == 'input.deliver':
                validate_delivery_acknowledgement(result)
            return result
        finally:
            self.pending.pop(request_id, None)
            if not future.done():
                future.cancel()

    async def disconnect(self):
        if self.closed:
            return
        self.closed = True
        self._fail_pending(ConnectionError('Connector WebSocket disconnected'))
        try:
            await self.websocket.close(code=1000)
        except Exception:
            pass

    def _fail_pending(self, error):
        for _, future in self.pending.values():
            if not future.done():
                future.set_exception(error)

    def _response(self, request_id, result, remote_error):
        pending = self.pending.pop(request_id, None)
        if pending is None:
            logger.debug('Ignoring late or unmatched protocol {} response', CONNECTOR_PROTOCOL)
            return
        event, future = pending
        if future.done():
            logger.debug('Ignoring duplicate protocol {} response', CONNECTOR_PROTOCOL)
            return
        if remote_error is not NO_REMOTE_ERROR:
            if (not isinstance(remote_error, dict) or type(remote_error.get('code')) is not int
                    or not isinstance(remote_error.get('message'), str)
                    or len(remote_error['message']) > 256):
                error = (InvalidConnectorAcknowledgement('Invalid input.deliver error response')
                         if event == 'input.deliver' else WireError('Invalid JSON-RPC error response'))
            elif event == 'input.deliver':
                error = InvalidConnectorAcknowledgement('Connector rejected input.deliver')
            else:
                error = RemoteRPCError(remote_error['code'], remote_error['message'])
            future.set_exception(error)
        else:
            future.set_result(result)

    async def _reply(self, request_id, result=None, error=None):
        if request_id is None:
            return
        if error is None:
            await self._write({'jsonrpc': '2.0', 'id': request_id, 'result': result})
        else:
            await self._write(_error(request_id, error[0], error[1]))

    async def _run_handler(self, request_id, method, params, dispatch):
        try:
            result = await asyncio.wait_for(dispatch(method, params), timeout=HANDLER_TIMEOUT_SECONDS)
        except asyncio.TimeoutError:
            await self._reply(request_id, error=(-32001, 'Request timed out'))
        except WireError as error:
            await self._reply(request_id, error=(-32602, str(error)))
        except RemoteRPCError as error:
            await self._reply(request_id, error=(error.code, str(error)))
        except Exception as error:
            logger.warning('Protocol 3 connector handler {} failed: {}', method, type(error).__name__)
            await self._reply(request_id, error=(-32603, 'Internal error'))
        else:
            try:
                await self._reply(request_id, result=result)
            except WireError:
                await self._reply(request_id, error=(-32603, 'Response exceeds the frame limit'))
        finally:
            if request_id is not None:
                self.incoming_ids.discard(request_id)

    async def run(self, dispatch):
        try:
            while True:
                message = await self.websocket.receive()
                if message['type'] == 'websocket.disconnect':
                    return
                if message.get('bytes') is not None:
                    await self.websocket.close(code=1003)
                    return
                try:
                    kind, request_id, value, extra = _decode(message.get('text'))
                except FrameTooLarge:
                    await self.websocket.close(code=1009)
                    return
                except WireError:
                    await self.websocket.close(code=1002)
                    return
                if kind == 'response':
                    self._response(request_id, value, extra)
                    continue
                if request_id is not None and request_id in self.incoming_ids:
                    await self.websocket.close(code=1002)
                    return
                if len(self.handlers) >= MAX_PENDING_HANDLERS:
                    await self._reply(request_id, error=(-32000, 'Connector request capacity reached'))
                    continue
                if request_id is not None:
                    self.incoming_ids.add(request_id)
                task = asyncio.create_task(self._run_handler(request_id, value, extra, dispatch))
                self.handlers.add(task)
                task.add_done_callback(self.handlers.discard)
        except WebSocketDisconnect:
            return
        finally:
            self.closed = True
            self._fail_pending(ConnectionError('Connector WebSocket disconnected'))
            handlers = list(self.handlers)
            for task in handlers:
                task.cancel()
            if handlers:
                await asyncio.gather(*handlers, return_exceptions=True)


def mount_connector_websocket(app, control):
    """Add the v3 protocol to the app; LocalOnly makes this route UDS-only."""

    @app.websocket(PATH)
    async def connector_v3(websocket: WebSocket):
        await websocket.accept()
        peer = None
        connector_id = None
        try:
            try:
                first = await asyncio.wait_for(websocket.receive(), timeout=HELLO_TIMEOUT_SECONDS)
                if first['type'] != 'websocket.receive' or first.get('bytes') is not None:
                    raise WireError('Expected a connector.hello text frame')
                kind, request_id, method, params = _decode(first.get('text'))
                if kind != 'request' or request_id is None or method != 'connector.hello':
                    raise WireError('First request must be connector.hello')
            except FrameTooLarge:
                await websocket.close(code=1009)
                return
            except (asyncio.TimeoutError, WireError):
                await websocket.close(code=1008)
                return

            connector_id = params.get('connector_id')
            token = params.get('token')
            if (type(params.get('protocol')) is not int or params['protocol'] != CONNECTOR_PROTOCOL
                    or not isinstance(connector_id, str) or not connector_id or len(connector_id) > 200
                    or not isinstance(token, str) or not token or len(token) > 512):
                await websocket.send_text(_encode(_error(request_id, -32002, 'Protocol 3 hello is required')))
                await websocket.close(code=1008)
                return
            try:
                authenticated = control.journal.authenticate_connector(connector_id, token, params)
            except Exception as error:
                logger.warning('Protocol 3 connector authentication failed: {}', type(error).__name__)
                authenticated = False
            if not authenticated:
                await websocket.send_text(_encode(_error(request_id, -32001, 'Connector credential refused')))
                await websocket.close(code=1008)
                return

            control.identity = {key: params[key] for key in ('host', 'platform', 'version', 'harnesses')
                                if params.get(key)}
            peer = WebSocketPeer(websocket)
            await control.attach(connector_id, peer)
            await peer._write({'jsonrpc': '2.0', 'id': request_id, 'result': {'protocol': CONNECTOR_PROTOCOL}})
            await peer.send('connector.welcome', {'protocol': CONNECTOR_PROTOCOL})
            if control.rendezvous is not None:
                await peer.send('node.rendezvous', control.rendezvous)

            async def dispatch(event, data):
                if event == 'binding.register':
                    try:
                        return await control.register(connector_id, data)
                    except ValueError as error:
                        return {'error': str(error)[:1000]}
                if event == 'binding.unregister':
                    await control.unregister(connector_id, data)
                    return None
                if event == 'speech.publish':
                    if (not isinstance(data.get('event_id'), str) or not data['event_id'] or len(data['event_id']) > 200
                            or not isinstance(data.get('utterance_id'), str) or not data['utterance_id']
                            or len(data['utterance_id']) > 200):
                        raise WireError('speech.publish requires bounded event_id and utterance_id strings')
                    return await control.speech(connector_id, data, protocol=CONNECTOR_PROTOCOL)
                if event == 'input.working':
                    await control.working(connector_id, data)
                    return None
                if event == 'input.engine':
                    await control.engine(connector_id, data)
                    return None
                if event == 'input.read':
                    await control.read(connector_id, data)
                    return None
                if event == 'device.pairing_code':
                    devices = getattr(app.state, 'devices', None)
                    if devices is None:
                        return {'error': 'This core does not pair devices.'}
                    try:
                        return devices.issue_code()
                    except (OSError, ValueError) as error:
                        logger.warning('Could not issue a device pairing code: {}', type(error).__name__)
                        return {'error': f'Could not issue a pairing code: {error}'[:1000]}
                raise RemoteRPCError(-32601, f'Method not found: {event}')

            await peer.run(dispatch)
        except (WebSocketDisconnect, ConnectionError, RuntimeError):
            pass
        finally:
            if peer is not None:
                peer._fail_pending(ConnectionError('Connector WebSocket disconnected'))
                control.detach(connector_id, peer)
                await peer.disconnect()

    app.state.connector_v3 = connector_v3
    return connector_v3
