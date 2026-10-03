"""Offline T6 contract against the Core binary and pinned Python Socket.IO peer."""

import asyncio
import http.client
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import socketio
import uvicorn

CORE = Path(sys.argv[1]).resolve()
assert CORE.is_file(), f"required Core binary missing: {CORE}"


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=5)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(str(self.path))


def request(path, method, uri, body=None):
    conn = UnixHTTP(path)
    conn.request(method, uri, json.dumps(body).encode() if body is not None else None,
                 {"Host": "localhost", "Content-Type": "application/json"})
    response = conn.getresponse()
    payload = response.read()
    conn.close()
    return response.status, json.loads(payload) if payload else None


async def until(check, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        await asyncio.sleep(0.05)
    raise AssertionError("timed out waiting for required relay state")


class RoomPeer:
    def __init__(self):
        self.server = socketio.AsyncServer(async_mode="asgi", namespaces=["/nodes"])
        self.auth = None
        self.sid = None
        self.frames = []
        self.closed = []
        self.connections = 0

        @self.server.event(namespace="/nodes")
        async def connect(sid, environ, auth):
            self.connections += 1
            self.auth = auth
            self.sid = sid
            await self.server.emit("node.welcome", {"protocol": 3, "public_url": "https://room.example"},
                                   to=sid, namespace="/nodes")

        @self.server.on("relay.data", namespace="/nodes")
        async def relay_data(sid, data):
            self.frames.append(data)

        @self.server.on("relay.close", namespace="/nodes")
        async def relay_close(sid, data):
            self.closed.append(data)

    async def ask(self, event, payload):
        return await self.server.call(event, payload, to=self.sid, namespace="/nodes", timeout=10)

    async def tell(self, event, payload):
        await self.server.emit(event, payload, to=self.sid, namespace="/nodes")


async def main():
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        room_port, core_port = free_port(), free_port()
        peer = RoomPeer()
        server = uvicorn.Server(uvicorn.Config(
            socketio.ASGIApp(peer.server, socketio_path="/api/connectors/link"),
            host="127.0.0.1", port=room_port, log_level="error"))
        room_task = asyncio.create_task(server.serve())
        core = None
        try:
            await until(lambda: server.started)
            pairing = root / "credentials.json"
            pairing.write_text(json.dumps({"url": f"http://127.0.0.1:{room_port}",
                                           "connector_id": "node-1", "token": "fixture-room-token",
                                           "dial_key": "fixture-dial-key", "protocol": 3}))
            data_dir = root / "core"
            env = dict(os.environ)
            env.pop("OTEL_EXPORTER_OTLP_ENDPOINT", None)
            env.pop("VOICE_STT_API_KEY", None)
            env.pop("VOICE_ELEVENLABS_API_KEY", None)
            core = subprocess.Popen([str(CORE), "--data-dir", str(data_dir), "--port", str(core_port),
                                     "--room-credential", str(pairing), "--idle-exit", "0"],
                                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                    stderr=subprocess.PIPE, env=env)
            await until(lambda: (data_dir / "core.json").exists() or core.poll() is not None)
            assert core.poll() is None, "Core exited before readiness"
            await until(lambda: peer.sid)
            assert (peer.auth["connector_id"], peer.auth["token"], peer.auth["protocol"]) == (
                "node-1", "fixture-room-token", 3)
            assert peer.auth.get("core")

            local = data_dir / "local.sock"
            status, paired = await asyncio.to_thread(request, local, "POST", "/api/device/local/pair", {"name": "T6 peer"})
            assert status == 200, (status, paired)
            token = paired["token"]

            async def admission():
                answer = await peer.ask("relay.http", {"method": "GET", "path": "/api/presentation/admission",
                                                       "headers": {"accept": "application/json",
                                                                   "authorization": f"Bearer {token}"}})
                assert answer["status"] == 200, answer
                assert isinstance(answer["body"], bytes), "HTTP body lost binary ACK attachment"
                assert json.loads(answer["body"])["admitted"] is True
                return answer

            await asyncio.gather(*(admission() for _ in range(16)))
            for path in ("/api/connectors/link", "/api/rendezvous", "/api/device/local/pair",
                         "/api/device/%2e%2e/connectors/link", "/api/models/../rendezvous",
                         "/api/presentation/%252e%252e/connectors"):
                answer = await peer.ask("relay.http", {"method": "GET", "path": path})
                assert answer["status"] == 404, (path, answer)

            unauthorized = await peer.ask("relay.http", {"method": "GET", "path": "/api/host/agents"})
            assert unauthorized["status"] == 401, unauthorized
            report = {"kind": "fixture", "message": "binary body reached Core"}
            recorded = await peer.ask("relay.http", {"method": "POST", "path": "/api/presentation/client-error",
                                                      "headers": {"authorization": f"Bearer {token}",
                                                                  "content-type": "application/json"},
                                                      "body": json.dumps(report).encode(),
                                                      "ignored": {"nested": [b"second-attachment"]}})
            assert recorded["status"] == 200, recorded
            assert json.loads(recorded["body"])["status"] == "recorded"
            snapshot = await peer.ask("relay.http", {"method": "GET", "path": "/api/presentation",
                                                      "headers": {"authorization": f"Bearer {token}"}})
            assert snapshot["status"] == 200, snapshot
            assert json.loads(snapshot["body"])["room"]["client_errors"][-1]["message"] == report["message"]

            opened = await peer.ask("relay.open", {"channel": "t6-call", "path": "/api/presentation/ws",
                                                   "protocols": ["sidevoice", f"sidevoice.token.{token}"]})
            assert opened == {"ok": True}, opened
            await peer.tell("relay.data", {"channel": "t6-call", "data": json.dumps({
                "type": "client-ready", "data": {"settings": {"turn_end_mode": "timer"}}})})
            await until(lambda: any(isinstance(frame.get("data"), str) and
                                    json.loads(frame["data"]).get("type") == "voice-session"
                                    for frame in peer.frames))
            await peer.tell("relay.data", {"channel": "t6-call", "data": b"\0\0" * 320})
            await peer.tell("relay.close", {"channel": "t6-call", "code": 1000})
            denied = await peer.ask("relay.open", {"channel": "not-call", "path": "/api/connectors/v3"})
            assert denied["ok"] is False, denied

            wrong = socketio.AsyncClient(reconnection=False)
            try:
                try:
                    await wrong.connect(f"http://127.0.0.1:{core_port}",
                                        socketio_path="/api/rendezvous/link", namespaces=["/room"],
                                        transports=["websocket"],
                                        auth={"connector_id": "node-1", "dial_key": "wrong"}, wait_timeout=5)
                except socketio.exceptions.ConnectionError:
                    pass
                else:
                    raise AssertionError("wrong dial key accepted")
            finally:
                await wrong.disconnect()

            dial = socketio.AsyncClient(reconnection=False)
            hello = asyncio.get_running_loop().create_future()

            @dial.on("node.hello", namespace="/room")
            async def node_hello(data):
                hello.set_result(data)
                return {"protocol": 3}

            await dial.connect(f"http://127.0.0.1:{core_port}",
                               socketio_path="/api/rendezvous/link", namespaces=["/room"],
                               transports=["websocket"],
                               auth={"connector_id": "node-1", "dial_key": "fixture-dial-key"}, wait_timeout=5)
            try:
                proof = await asyncio.wait_for(hello, 10)
                assert (proof["connector_id"], proof["token"], proof["protocol"]) == (
                    "node-1", "fixture-room-token", 3)
                dial_answer = await dial.call("relay.http", {"method": "GET",
                                                             "path": "/api/presentation/admission",
                                                             "headers": {"authorization": f"Bearer {token}"}},
                                              namespace="/room", timeout=10)
                assert dial_answer["status"] == 200 and isinstance(dial_answer["body"], bytes)
            finally:
                await dial.disconnect()

            # A server-side disconnect must lead to another authenticated /nodes session.
            previous = peer.sid
            await peer.server.disconnect(previous, namespace="/nodes")
            await until(lambda: peer.connections >= 2 and peer.sid != previous, timeout=20)
            await admission()
            print(json.dumps({"ok": True, "peer": "python-socketio==5.17.0", "parallel_acks": 16,
                              "http_binary": True, "ws_binary": True, "dial_ack": True,
                              "reconnected": True,
                              "telemetry_endpoint": "unset"}))
        finally:
            if core is not None and core.poll() is None:
                core.send_signal(signal.SIGTERM)
                try:
                    await asyncio.to_thread(core.wait, 8)
                except subprocess.TimeoutExpired:
                    core.kill()
                    await asyncio.to_thread(core.wait)
            server.should_exit = True
            await room_task


if __name__ == "__main__":
    asyncio.run(main())
