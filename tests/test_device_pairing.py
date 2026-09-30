"""Device pairing, the node's side (rubasace/sidevoice `docs/DEVICE_PAIRING.md`): the node's identity, the
pairing code its connector asks for, redeeming it once, the token every route but a few requires (the call
socket's in a subprotocol), the identity proof a client pins, the devices list and revoke, and the relay
carrying all of it from the room end to end."""
import asyncio
import base64
import hashlib
import json
import os
import stat
import tempfile
import time
import unittest
import uuid
from pathlib import Path

import aiohttp
import socketio
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import encode_dss_signature
from starlette.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from sidevoice_core.control.connectors import PROTOCOL as CONNECTOR_PROTOCOL
from sidevoice_core.control.devices import (CODE_TTL, DeviceRegistry, IdentityError, NodeDevices, b64url,
                                            decode_code, encode_code)
from sidevoice_core.control.history import RoomHistory
from sidevoice_core.control.room import Room
from sidevoice_core.server.app import create_app
from test_rendezvous import NodeTest, until

BASE = 'http://127.0.0.1:8768'
APP = 'tauri://localhost'
SOCKET = 'ws://127.0.0.1:8768/api/presentation/ws'   # absolute: a relative one is joined onto ws://testserver
PAYLOAD_KEYS = {'v', 'fp', 'host', 'urls', 'rv', 'secret', 'exp'}


def fingerprint_of(public_key):
    return b64url(hashlib.sha256(base64.b64decode(public_key)).digest())


def verify(public_key, nonce, signature):
    """What a client does with its pinned key: P1363 (r‖s) back to DER, then ECDSA P-256 / SHA-256."""
    key = serialization.load_der_public_key(base64.b64decode(public_key))
    raw = base64.urlsafe_b64decode(signature + '=' * (-len(signature) % 4))
    if len(raw) != 64:
        raise AssertionError(f'a P1363 P-256 signature is 64 bytes, not {len(raw)}')
    der = encode_dss_signature(int.from_bytes(raw[:32], 'big'), int.from_bytes(raw[32:], 'big'))
    key.verify(der, ('sidevoice-node-identity:' + nonce).encode('utf8'), ec.ECDSA(hashes.SHA256()))


def auth(token):
    return {'Authorization': f'Bearer {token}'}


class StoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_the_identity_is_created_once_kept_private_and_stable(self):
        first = NodeDevices(self.root).identity
        path = self.root / 'node-identity.json'
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        saved = json.loads(path.read_text())
        self.assertEqual(set(saved), {'private_key_pem', 'created'})
        again = NodeDevices(self.root).identity
        self.assertEqual((again.public_key, again.fingerprint), (first.public_key, first.fingerprint), 'the key survives a restart')
        der = base64.b64decode(first.public_key)
        key = serialization.load_der_public_key(der)
        self.assertIsInstance(key.curve, ec.SECP256R1)
        self.assertEqual(first.fingerprint, b64url(hashlib.sha256(der).digest()))
        self.assertEqual(len(first.fingerprint), 43)
        self.assertEqual([p.name for p in self.root.iterdir()], ['node-identity.json'], 'no staging file is left behind')

    def test_an_identity_that_cannot_be_read_is_refused_never_replaced(self):
        path = self.root / 'node-identity.json'
        path.write_text('{"private_key_pem": "not a key"}')
        with self.assertRaises(IdentityError):
            NodeDevices(self.root).identity
        self.assertEqual(path.read_text(), '{"private_key_pem": "not a key"}')

    def test_a_code_is_the_payload_it_carries(self):
        store = NodeDevices(self.root)
        issued = store.issue_code(host='laptop', urls=['http://127.0.0.1:8768'], rv={'url': 'https://room.example', 'node': 'n-1'})
        self.assertTrue(issued['code'].startswith('SV1.'))
        self.assertNotIn('=', issued['code'])
        self.assertEqual(issued['expires_in'], 600)
        payload = decode_code(issued['code'])
        self.assertEqual(payload, issued['payload'])
        self.assertEqual(set(payload), PAYLOAD_KEYS)
        self.assertEqual((payload['v'], payload['fp'], payload['host']), (1, store.identity.fingerprint, 'laptop'))
        self.assertEqual(len(base64.urlsafe_b64decode(payload['secret'] + '==')), 16)
        self.assertAlmostEqual(payload['exp'], time.time() + CODE_TTL, delta=5)
        bare = store.issue_code(host=None, urls=[], rv=None)['payload']
        self.assertEqual((bare['host'], bare['rv'], bare['urls']), (None, None, []), 'absent values are null')
        self.assertEqual(decode_code(encode_code({'v': 1})), {'v': 1})
        for wrong in ('', 'SV2.e30', 'SV1.!!!', 'SV1.' + b64url(b'[1]')):
            with self.assertRaises(ValueError):
                decode_code(wrong)

    def test_only_a_handful_of_codes_are_outstanding_and_each_is_one_time(self):
        registry = DeviceRegistry(self.root / 'devices.json')
        issued = [registry.issue_secret()[0] for _ in range(6)]
        self.assertIsNone(registry.redeem(issued[0], 'x'), 'the oldest was dropped')
        self.assertIsNotNone(registry.redeem(issued[5], 'x'))
        self.assertIsNone(registry.redeem(issued[5], 'x'), 'one-time')

    def test_last_seen_moves_at_most_once_a_minute(self):
        now = [1_000_000.0]
        registry = DeviceRegistry(self.root / 'devices.json', clock=lambda: now[0])
        device_id, token = registry.redeem(registry.issue_secret()[0], 'phone')
        written = (self.root / 'devices.json').stat().st_mtime_ns
        now[0] += 30
        self.assertEqual(registry.authenticate(token), device_id)
        self.assertEqual(registry.devices[device_id]['last_seen'], 1_000_000)
        self.assertEqual((self.root / 'devices.json').stat().st_mtime_ns, written, 'not written inside the minute')
        now[0] += 31
        registry.authenticate(token)
        self.assertEqual(registry.devices[device_id]['last_seen'], 1_000_061)
        self.assertEqual(DeviceRegistry(self.root / 'devices.json').devices[device_id]['last_seen'], 1_000_061)


class NodeSurfaceTests(unittest.TestCase):
    """The node as `create_app` assembles it, device auth on — the way `server.__main__` always runs it."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.data = Path(self.temp.name) / 'core'
        self.room = Room(RoomHistory(self.data / 'room-state.json'))
        self.app = create_app(self.room, config={'VOICE_BROWSER_HEARTBEAT_SECONDS': '0', 'SIDEVOICE_CORE_DATA_DIR': str(self.data),
                                                 'SIDEVOICE_PUBLIC_URLS': ' https://node.example/ , ftp://not.a.web.url, https://node.example'})
        self.devices = self.app.state.devices
        self.devices.listen_url = BASE

    def client(self, base_url=BASE):
        return TestClient(self.app, base_url=base_url)

    def pair(self, client, name='Mi portátil'):
        code = self.devices.issue_code()
        return code, client.post('/api/device/pair', json={'secret': code['payload']['secret'], 'name': name})

    def paired(self, client, name='Mi portátil'):
        _, answer = self.pair(client, name)
        self.assertEqual(answer.status_code, 200, answer.text)
        return answer.json()

    def test_a_code_says_where_the_node_answers(self):
        payload = self.devices.issue_code()['payload']
        self.assertEqual(payload['urls'], [BASE, 'https://node.example'])
        self.assertIsNone(payload['rv'], 'no rendezvous, no room')
        self.assertEqual(payload['host'], self.devices.host())

    def test_a_code_is_redeemed_once_and_only_its_hash_is_kept(self):
        with self.client() as client:
            code, answer = self.pair(client)
            self.assertEqual(answer.status_code, 200, answer.text)
            redeemed = answer.json()
            self.assertEqual(set(redeemed), {'device_id', 'token', 'node'})
            uuid.UUID(redeemed['device_id'])
            self.assertEqual(len(base64.urlsafe_b64decode(redeemed['token'] + '=')), 32)
            node = redeemed['node']
            self.assertEqual(set(node), {'fingerprint', 'public_key', 'host'})
            self.assertEqual(fingerprint_of(node['public_key']), code['payload']['fp'], 'what a client checks before keeping the token')
            again = client.post('/api/device/pair', json={'secret': code['payload']['secret'], 'name': 'otra'})
            self.assertEqual(again.status_code, 403)
            self.assertIsInstance(again.json()['detail'], str)
            unknown = client.post('/api/device/pair', json={'secret': 'guessed', 'name': 'x'})
            self.assertEqual(unknown.status_code, 403)
            expired = self.devices.issue_code()
            self.devices.store.registry.clock = lambda: time.time() + CODE_TTL + 1
            late = client.post('/api/device/pair', json={'secret': expired['payload']['secret'], 'name': 'tarde'})
            self.assertEqual(late.status_code, 403)
        saved = self.data / 'devices.json'
        self.assertEqual(stat.S_IMODE(saved.stat().st_mode), 0o600)
        self.assertNotIn(redeemed['token'], saved.read_text())
        entry = json.loads(saved.read_text())['devices'][redeemed['device_id']]
        self.assertEqual(entry['token_hash'], hashlib.sha256(redeemed['token'].encode()).hexdigest())
        self.assertEqual(entry['name'], 'Mi portátil')

    def test_every_route_but_the_open_ones_needs_a_device_token(self):
        with self.client() as client:
            token = self.paired(client)['token']
            protected = [('GET', '/api/presentation/admission', None), ('GET', '/api/connectors', None),
                         ('POST', '/api/rendezvous/pair', {'room': 'https://room.example', 'code': 'ABCD'}),
                         ('GET', '/api/presentation/rtc/config', None), ('GET', '/api/device/devices', None),
                         ('DELETE', '/api/device/devices/nobody', None), ('GET', '/api/presentation/history', None),
                         ('GET', '/api/presentation/integrations', None),
                         ('PUT', '/api/presentation/integrations/openai', {'key': 'sk-guessed'}),
                         ('DELETE', '/api/presentation/integrations/openai', None)]
            for method, path, body in protected:
                with self.subTest(path=path):
                    for headers in ({}, auth('not-a-token'), {'Authorization': f'Basic {token}'}, {'Authorization': 'Bearer'}):
                        refused = client.request(method, path, json=body, headers={'Origin': APP, **headers})
                        self.assertEqual(refused.status_code, 401, (path, headers))
                        self.assertIsInstance(refused.json()['detail'], str)
                        self.assertEqual(refused.headers['www-authenticate'], 'Bearer')
                        self.assertEqual(refused.headers['access-control-allow-origin'], APP, 'the page can read why')
            # With the token each is its normal self.
            self.assertEqual(client.get('/api/presentation/admission', headers=auth(token)).json(), self.room.admission())
            self.assertEqual(client.get('/api/connectors', headers=auth(token)).status_code, 200)
            self.assertEqual(client.post('/api/rendezvous/pair', headers={'Origin': APP, **auth(token)},
                                         json={'room': 'https://room.example', 'code': 'ABCD'}).status_code, 503,
                             'past the token: no connector here to pair with')
            self.assertEqual(client.get('/api/presentation/rtc/config', headers=auth(token)).status_code, 200)
            self.assertEqual(client.get('/api/device/devices', headers={'authorization': f'bearer {token}'}).status_code, 200)
            # Open: what this is, the redemption, the identity proof, preflights, the connector's own link.
            what = client.get('/api/rendezvous')
            self.assertEqual(what.status_code, 200)
            self.assertEqual(what.json()['fingerprint'], self.devices.store.identity.fingerprint)
            self.assertEqual(client.get('/api/device/identity', params={'nonce': b64url(os.urandom(16))}).status_code, 200)
            self.assertEqual(client.post('/api/device/pair', json={'secret': 'nope'}).status_code, 403, 'refused for the secret, not the token')
            preflight = client.options('/api/presentation/text', headers={
                'Origin': APP, 'Access-Control-Request-Method': 'POST', 'Access-Control-Request-Headers': 'authorization, content-type'})
            self.assertEqual(preflight.status_code, 204)
            self.assertIn('authorization', preflight.headers['access-control-allow-headers'])
            self.assertEqual(client.get('/api/connectors/link/', params={'EIO': '4', 'transport': 'polling'}).status_code, 200,
                             'the connector link carries its own credential')
        with self.client('http://attacker.example') as client:
            self.assertEqual(client.get('/api/presentation/admission', headers=auth(token)).status_code, 421, 'a foreign Host goes first')

    def test_the_call_socket_takes_its_token_as_a_subprotocol(self):
        with self.client() as client:
            token = self.paired(client)['token']
            for offered in (['sidevoice'], ['sidevoice', 'sidevoice.token.guessed'], None):
                with self.subTest(offered=offered):
                    with self.assertRaises(WebSocketDisconnect) as closed:
                        with client.websocket_connect(SOCKET, subprotocols=offered) as ws:
                            ws.receive_text()
                    self.assertEqual(closed.exception.code, 4401)
            with client.websocket_connect(SOCKET, subprotocols=['sidevoice', f'sidevoice.token.{token}']) as ws:
                self.assertEqual(ws.accepted_subprotocol, 'sidevoice')
            with self.assertRaises(WebSocketDisconnect) as closed:
                with client.websocket_connect(SOCKET, subprotocols=['sidevoice'], headers=auth(token)) as ws:
                    ws.receive_text()
            self.assertEqual(closed.exception.code, 4401, 'the socket takes its token as a subprotocol, as a browser must send it')

    def test_the_identity_proof_verifies_with_the_pinned_key(self):
        with self.client() as client:
            nonce = b64url(os.urandom(32))
            answer = client.get('/api/device/identity', params={'nonce': nonce})
            self.assertEqual(answer.status_code, 200, answer.text)
            proof = answer.json()
            self.assertEqual(set(proof), {'fingerprint', 'public_key', 'host', 'signature'})
            self.assertEqual(fingerprint_of(proof['public_key']), proof['fingerprint'])
            verify(proof['public_key'], nonce, proof['signature'])
            with self.assertRaises(InvalidSignature):
                verify(proof['public_key'], b64url(os.urandom(32)), proof['signature'])
            padded = base64.urlsafe_b64encode(os.urandom(16)).decode()
            verify(proof['public_key'], padded, client.get('/api/device/identity', params={'nonce': padded}).json()['signature'])
            for wrong in ('', b64url(os.urandom(15)), b64url(os.urandom(65)), 'not base64url!!!!!!!!!!!!!'):
                with self.subTest(nonce=wrong):
                    self.assertEqual(client.get('/api/device/identity', params={'nonce': wrong}).status_code, 400)

    def test_the_devices_list_marks_the_asking_device_and_revoking_is_immediate(self):
        with self.client() as client:
            mine, other = self.paired(client, 'Mi portátil'), self.paired(client, 'El móvil')
            listed = client.get('/api/device/devices', headers=auth(mine['token'])).json()['devices']
            self.assertEqual([(d['id'], d['name'], d['current']) for d in listed],
                             [(mine['device_id'], 'Mi portátil', True), (other['device_id'], 'El móvil', False)])
            self.assertEqual(set(listed[0]), {'id', 'name', 'created', 'last_seen', 'current'})
            self.assertIsInstance(listed[0]['created'], int)
            gone = client.delete(f"/api/device/devices/{other['device_id']}", headers=auth(mine['token']))
            self.assertEqual((gone.status_code, gone.json()), (200, {'ok': True}))
            self.assertEqual(client.get('/api/device/devices', headers=auth(other['token'])).status_code, 401, 'at once')
            self.assertEqual(client.delete('/api/device/devices/nobody', headers=auth(mine['token'])).status_code, 404)
            self.assertEqual(client.delete(f"/api/device/devices/{mine['device_id']}", headers=auth(mine['token'])).status_code, 200,
                             'a device may revoke itself')
            self.assertEqual(client.get('/api/device/devices', headers=auth(mine['token'])).status_code, 401)
        self.assertEqual(NodeDevices(self.data).registry.devices, {}, 'and a restart does not bring them back')


    def test_revoking_a_device_ends_the_call_it_has_open(self):
        with self.client() as client:
            laptop, phone = self.paired(client, 'Portátil robado'), self.paired(client, 'El móvil')
            with client.websocket_connect(SOCKET, subprotocols=['sidevoice', f"sidevoice.token.{laptop['token']}"]) as ws:
                gone = client.delete(f"/api/device/devices/{laptop['device_id']}", headers=auth(phone['token']))
                self.assertEqual(gone.status_code, 200)
                with self.assertRaises(WebSocketDisconnect) as closed:
                    ws.receive_text()
                self.assertEqual(closed.exception.code, 4401, 'a revoked device stops speaking and listening now')
            self.assertEqual(self.devices.calls, {}, 'nothing is kept of the ended call')


class LinkedNodeTest(NodeTest):
    """A node with device auth on, linked with a stand-in room, and this machine's connector linked to it."""
    device_auth = True

    async def connector(self):
        credential = self.node_room.journal.redeem_pairing_code(self.node_room.journal.create_pairing_code())
        connector = socketio.AsyncClient(reconnection=False)
        await connector.connect(f'http://127.0.0.1:{self.node_port}', socketio_path='/api/connectors/link',
                                namespaces=['/connectors'], transports=['websocket'],
                                auth={'connector_id': credential[0], 'token': credential[1], 'protocol': CONNECTOR_PROTOCOL,
                                      'host': 'this-laptop', 'platform': 'Linux x86_64', 'version': '0.7.0'})
        return connector

    async def redeem_through_the_relay(self, secret):
        answer = await self.room.ask('relay.http', {'method': 'POST', 'path': '/api/device/pair', 'query': '',
                                                    'headers': {'content-type': 'application/json', 'origin': 'https://room.example'},
                                                    'body': json.dumps({'secret': secret, 'name': 'Navegador'}).encode()})
        self.assertEqual(answer['status'], 200, answer['body'])
        return json.loads(answer['body'])


class ConnectorCodeTests(LinkedNodeTest):
    room_public_url = 'https://room.example'

    async def test_the_connector_asks_for_a_code_and_a_device_redeems_it(self):
        connector = await self.connector()
        try:
            await until(lambda: self.rendezvous.state['connected'])
            issued = await connector.call('device.pairing_code', {}, namespace='/connectors', timeout=10)
        finally:
            await connector.disconnect()
        self.assertEqual(set(issued), {'code', 'payload', 'expires_in'})
        self.assertEqual(issued['expires_in'], 600)
        payload = decode_code(issued['code'])
        self.assertEqual(payload, issued['payload'])
        self.assertEqual(set(payload), PAYLOAD_KEYS)
        self.assertEqual(payload['urls'], [f'http://127.0.0.1:{self.node_port}'])
        self.assertEqual(payload['host'], 'this-laptop', 'the machine as its connector names it')
        self.assertEqual(payload['rv'], {'url': 'https://room.example', 'node': 'machine-1'},
                         "the room's public origin, as its welcome said it")
        async with aiohttp.ClientSession() as http:
            async with http.post(payload['urls'][0] + '/api/device/pair', json={'secret': payload['secret'], 'name': 'Escritorio'}) as answer:
                self.assertEqual(answer.status, 200, await answer.text())
                redeemed = await answer.json()
            self.assertEqual(fingerprint_of(redeemed['node']['public_key']), payload['fp'])
            async with http.get(payload['urls'][0] + '/api/device/devices', headers=auth(redeemed['token'])) as answer:
                self.assertEqual(answer.status, 200)
        # Re-paired elsewhere: the old room's word about its address goes with the old link.
        self.pairing.write_text(json.dumps({'url': 'http://127.0.0.1:9', 'connector_id': 'machine-2', 'token': 't'}))
        self.assertEqual(self.app.state.devices.issue_code()['payload']['rv'], {'url': 'http://127.0.0.1:9', 'node': 'machine-2'},
                         'not the old room\'s address, even before the old link is let go')
        await until(lambda: self.rendezvous.public_url is None, timeout=15)


class RelayTests(LinkedNodeTest):
    async def test_a_code_names_the_pairing_s_origin_when_the_room_said_none(self):
        await until(lambda: self.rendezvous.state['connected'])
        self.assertEqual(self.app.state.devices.issue_code()['payload']['rv'],
                         {'url': f'http://127.0.0.1:{self.room_port}', 'node': 'machine-1'})

    async def test_relayed_requests_carry_the_token_and_the_node_checks_it(self):
        await until(lambda: self.room.sid)
        issued = self.app.state.devices.issue_code()
        redeemed = await self.redeem_through_the_relay(issued['payload']['secret'])
        token = redeemed['token']

        async def relayed(path, headers=None, query=''):
            return await self.room.ask('relay.http', {'method': 'GET', 'path': path, 'query': query,
                                                      'headers': {'accept': 'application/json', **(headers or {})}, 'body': None})
        nonce = b64url(os.urandom(16))
        proof = await relayed('/api/device/identity', query=f'nonce={nonce}')
        self.assertEqual(proof['status'], 200, 'the identity proof is relayed and open')
        verify(redeemed['node']['public_key'], nonce, json.loads(proof['body'])['signature'])
        self.assertEqual((await relayed('/api/presentation/admission'))['status'], 401)
        self.assertEqual((await relayed('/api/presentation/admission', {'authorization': 'Bearer guessed'}))['status'], 401)
        allowed = await relayed('/api/presentation/admission', {'authorization': f'Bearer {token}'})
        self.assertEqual(allowed['status'], 200)
        self.assertEqual(json.loads(allowed['body']), self.node_room.admission())
        listed = await relayed('/api/device/devices', {'authorization': f'Bearer {token}'})
        self.assertEqual([d['current'] for d in json.loads(listed['body'])['devices']], [True])
        self.assertEqual((await relayed('/api/connectors', {'authorization': f'Bearer {token}'}))['status'], 404,
                         'still only the client surface is relayed')

    async def test_encoded_dot_segments_do_not_leave_the_relayed_surface(self):
        await until(lambda: self.room.sid)
        for path in ('/api/device/%2e%2e/connectors/link/', '/api/device/%252e%252e/connectors/link/',
                     '/api/device/%2e%2e/%2e%2e/api/rendezvous', '/api/presentation/%2E%2E%2Fconnectors', '/api/device/..%2f..%2fapi'):
            with self.subTest(path=path):
                answer = await self.room.ask('relay.http', {'method': 'GET', 'path': path, 'query': 'EIO=4&transport=polling',
                                                            'headers': {'accept': 'application/json'}, 'body': None})
                self.assertEqual(answer['status'], 404, 'only the client surface is relayed, however it is spelled')

    async def test_a_relayed_call_socket_offers_the_token_and_without_it_is_closed_4401(self):
        await until(lambda: self.room.sid)
        token = (await self.redeem_through_the_relay(self.app.state.devices.issue_code()['payload']['secret']))['token']
        for channel, protocols in (('c-none', None), ('c-bare', ['sidevoice']), ('c-wrong', ['sidevoice', 'sidevoice.token.guessed'])):
            opened = await self.room.ask('relay.open', {'channel': channel, 'path': '/api/presentation/ws', 'query': '',
                                                        **({'protocols': protocols} if protocols else {})})
            self.assertEqual(opened, {'ok': True})
            closed = await until(lambda: next((c for c in self.room.closed if c['channel'] == channel), None))
            self.assertEqual(closed['code'], 4401, 'the room closes the browser with the node\'s own code')
        opened = await self.room.ask('relay.open', {'channel': 'c-1', 'path': '/api/presentation/ws', 'query': '',
                                                    'protocols': ['sidevoice', f'sidevoice.token.{token}', 'bad\r\nheader: x']})
        self.assertEqual(opened, {'ok': True})
        await self.room.tell('relay.data', {'channel': 'c-1', 'data': json.dumps(
            {'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'x', 'data': {'settings': {'turn_end_mode': 'timer'}}})})
        session = await until(lambda: next((json.loads(f['data'])['data'] for f in self.room.frames if f['channel'] == 'c-1'
                                            and isinstance(f['data'], str) and json.loads(f['data']).get('type') == 'voice-session'), None), timeout=30)
        self.assertIn(session['session_id'], self.node_room.clients, 'the relayed browser, with its token, is in the call')
        await self.room.tell('relay.close', {'channel': 'c-1', 'code': 1000})
        await until(lambda: not self.node_room.clients, timeout=15)

    async def test_a_real_client_without_a_token_sees_4401(self):
        """Why the node accepts before closing: a socket closed before its handshake completes reaches a real
        client as an HTTP 403, and the page would never learn it has to pair again."""
        async with aiohttp.ClientSession() as http:
            ws = await http.ws_connect(f'ws://127.0.0.1:{self.node_port}/api/presentation/ws', protocols=['sidevoice'])
            self.assertEqual(ws.protocol, 'sidevoice')
            message = await asyncio.wait_for(ws.receive(), 10)
            self.assertEqual((message.type, ws.close_code), (aiohttp.WSMsgType.CLOSE, 4401))
            await ws.close()


if __name__ == '__main__':
    unittest.main()
