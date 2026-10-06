"""Offline functional T3 contract with pinned JS v2 and Rust v3 connector peers."""

import asyncio
import concurrent.futures
import http.client
import json
import os
import queue
import select
import signal
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

import websockets

CORE = Path(sys.argv[1]).resolve()
JS_LINK = Path(sys.argv[2]).resolve()
RUST_PEER = Path(sys.argv[3]).resolve()
for required in (CORE, JS_LINK, RUST_PEER):
    assert required.is_file(), f"required pinned peer missing: {required}"


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=5)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(str(self.path))


def request(port, method, path, *, token=None, body=None, unix=None, timeout=5):
    connection = UnixHTTP(unix) if unix else http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    headers = {"Host": "localhost", "Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    connection.request(method, path, json.dumps(body) if body is not None else None, headers)
    response = connection.getresponse()
    raw = response.read()
    value = json.loads(raw) if raw else None
    connection.close()
    return response.status, value


def until(predicate, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError("timed out waiting for contract state")


class LineProcess:
    def __init__(self, command, *, env=None):
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, text=True, bufsize=1, env=env)
        self.lines = queue.Queue()
        self.seen = []
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            value = json.loads(line)
            self.lines.put(value)

    def send(self, value):
        self.process.stdin.write(json.dumps(value) + "\n")
        self.process.stdin.flush()

    def event(self, name, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for index, value in enumerate(self.seen):
                if value.get("event") == name:
                    return self.seen.pop(index)
            try:
                value = self.lines.get(timeout=max(0.01, deadline-time.monotonic()))
            except queue.Empty:
                break
            if value.get("event") == "error":
                raise AssertionError(value)
            self.seen.append(value)
        raise AssertionError(f"pinned peer did not report {name}; seen={self.seen}")

    def method(self, name, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            event = self.event("from-core", timeout=deadline - time.monotonic())
            if event["method"] == name:
                return event
        raise AssertionError(f"pinned peer did not receive {name}")

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)


class UnixBridge(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, unix):
        self.unix = str(unix)
        super().__init__(("127.0.0.1", 0), BridgeHandler)
        threading.Thread(target=self.serve_forever, daemon=True).start()


class BridgeHandler(socketserver.BaseRequestHandler):
    def handle(self):
        upstream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        upstream.connect(self.server.unix)
        sockets = (self.request, upstream)
        try:
            while True:
                readable, _, _ = select.select(sockets, [], [], 5)
                for source in readable:
                    payload = source.recv(65536)
                    if not payload:
                        return
                    (upstream if source is self.request else self.request).sendall(payload)
        finally:
            upstream.close()


async def frame(ws, kind, *, status=None, timeout=6):
    deadline = time.monotonic() + timeout
    seen = []
    while time.monotonic() < deadline:
        try:
            value = json.loads(await asyncio.wait_for(ws.recv(), deadline-time.monotonic()))
        except TimeoutError as error:
            raise AssertionError(f"missing {kind}/{status}; frames={seen}") from error
        seen.append(value)
        if value.get("type") == kind and (status is None or value.get("data", {}).get("status") == status):
            return value["data"]
    raise AssertionError(f"missing {kind}/{status}; frames={seen}")


async def rpc_frame(ws, *, method=None, rid=None, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = json.loads(await asyncio.wait_for(ws.recv(), deadline - time.monotonic()))
        if (method is None or value.get("method") == method) and (rid is None or value.get("id") == rid):
            return value
    raise AssertionError(f"missing JSON-RPC method={method} id={rid}")


class ProofIPC:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(8)
        self.socket.connect(str(path))
        self.file = self.socket.makefile("rwb", buffering=0)
        self.seq = 0

    def call(self, method, params):
        self.seq += 1
        self.file.write((json.dumps({"id": self.seq, "method": method, "params": params})+"\n").encode())
        result = json.loads(self.file.readline())
        assert result["id"] == self.seq and result["ok"], result
        return result["result"]

    def close(self):
        self.socket.close()


async def main():
    with tempfile.TemporaryDirectory(prefix="sidevoice-t3-") as temporary:
        root = Path(temporary)
        data = root / "rust-peer"
        core_data = data / "core"
        codex = root / "codex"
        for directory in (data, core_data, codex):
            directory.mkdir(mode=0o700)
        core = subprocess.Popen([str(CORE), "--data-dir", str(core_data), "--port", "0", "--idle-exit", "0"],
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        peers = []
        bridge = None
        try:
            try:
                ready = until(lambda: json.loads((core_data / "core.json").read_text()) if (core_data / "core.json").exists() else None)
            except AssertionError as error:
                if core.poll() is None:
                    core.terminate()
                    core.wait(timeout=5)
                raise AssertionError(f"{error}; Core exited {core.returncode}: {core.stderr.read()}") from error
            port, uds = ready["port"], core_data / "local.sock"
            assert 2 in ready["connector_protocols"] and 3 in ready["connector_protocols"]
            assert request(port, "GET", "/api/connectors/v3")[0] == 404, "TCP gained connector authority"
            pair = request(port, "POST", "/api/device/local/pair", unix=uds, body={"name": "T3 browser"})
            assert pair[0] == 200, pair
            token = pair[1]["token"]
            bridge = UnixBridge(uds)
            origin = f"http://127.0.0.1:{bridge.server_address[1]}"
            async with websockets.connect(f"ws://127.0.0.1:{port}/api/presentation/ws",
                                          subprotocols=["sidevoice", f"sidevoice.token.{token}"]) as ws:
                await ws.send(json.dumps({"type": "voice-hello", "data": {}}))
                session = (await frame(ws, "voice-session"))["session_id"]
                browser_url = f"ws://127.0.0.1:{port}/api/presentation/ws"
                browser_protocols = ["sidevoice", f"sidevoice.token.{token}"]
                def active_calls():
                    return request(port, "GET", "/api/local/health", unix=uds)[1]["calls"]

                # All three early exits must release their revocation registrations.
                for _ in range(12):
                    async with websockets.connect(browser_url, subprotocols=browser_protocols) as refused:
                        await refused.send(b"invalid hello")
                        await refused.wait_closed()
                    until(lambda: active_calls() == 1)
                    async with websockets.connect(browser_url, subprotocols=browser_protocols) as refused:
                        await refused.send(json.dumps({"type": "voice-hello", "data": {
                            "settings": {"tts": {"place": "host", "model": "kokoro-82m-v1.0"}}}}))
                        assert (await frame(refused, "error"))["key"] == "place_host_unavailable"
                        await refused.wait_closed()
                        assert refused.close_code == 1008
                    until(lambda: active_calls() == 1)
                admitted = []
                try:
                    for _ in range(7):
                        another = await websockets.connect(browser_url, subprotocols=browser_protocols)
                        admitted.append(another)
                        await another.send(json.dumps({"type": "voice-hello", "data": {}}))
                        await frame(another, "voice-session")
                    assert active_calls() == 8
                    for _ in range(12):
                        async with websockets.connect(browser_url, subprotocols=browser_protocols) as refused:
                            await refused.send(json.dumps({"type": "voice-hello", "data": {}}))
                            assert (await frame(refused, "error"))["reason"] == "room_is_full"
                            await refused.wait_closed()
                            assert refused.close_code == 1013
                        until(lambda: active_calls() == 8)
                finally:
                    for another in admitted:
                        await another.close()
                until(lambda: active_calls() == 1)
                js = LineProcess(["node", str(Path(__file__).with_name("rust_t3_v2_peer.mjs")),
                                  str(JS_LINK), origin, ready["connector_id"], ready["token"]])
                peers.append(js)
                assert js.event("welcome")["welcome"]["protocol"] == 2
                binding = js.event("binding")["binding"]
                assert binding["thread"] == "t3-js-thread"
                assert request(port, "GET", "/api/presentation/participants", token=token)[1]["participants"][0]["thread_id"] == "t3-js-thread"
                selected = request(port, "POST", "/api/presentation/select", token=token,
                                   body={"session_id": session, "thread_id": "t3-js-thread"})
                assert selected[0] == 200 and selected[1]["binding"]["binding_id"]
                focus = selected[1]["binding"]["binding_id"]
                revision = request(port, "GET", f"/api/presentation?session_id={session}", token=token)[1]["room"]["revision"]
                message_id = str(uuid.uuid4())
                sent = request(port, "POST", "/api/presentation/text", token=token,
                               body={"text": "Hello from pinned browser contract", "session_id": session,
                                     "thread_id": "t3-js-thread", "binding_id": focus, "message_id": message_id})
                assert sent[0] == 200 and sent[1]["revision"] == revision, sent
                assert (await frame(ws, "voice-input-receipt", status="pending"))["history_id"] == sent[1]["id"]
                js.method("input.deliver")
                assert (await frame(ws, "voice-input-receipt", status="delivered"))["history_id"] == sent[1]["id"]
                js.send({"op": "read", "message_id": message_id})
                assert (await frame(ws, "voice-input-receipt", status="read"))["history_id"] == sent[1]["id"]
                js.send({"op": "working", "working": True})
                assert (await frame(ws, "voice-conversation"))["working"] is True
                js.send({"op": "publish", "session_id": session, "revision": revision,
                         "event_id": "t3-js-event", "utterance_id": "t3-js-utterance", "text": "Reply from JS peer"})
                published = js.event("published")["answer"]
                assert published["status"] == "queued" and published["text_saved"] is True, published
                speech = await frame(ws, "voice-speech")
                assert speech["text"] == "Reply from JS peer" and speech["session_id"] == session
                receipt = {"session_id": session, "utterance_id": "t3-js-utterance", "revision": speech["revision"]}
                assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                               body={**receipt, "status": "playing"})[0] == 200
                assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                               body={**receipt, "status": "playback_finished"})[0] == 200
                assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                               body={**receipt, "status": "playing"})[0] == 409
                history = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
                assert [row["seq"] for row in history] == [1, 2], history
                assert history[0]["status"] == "read" and history[1]["status"] == "playback_finished", history
                # One browser hears replies in order; a queued reply waits for the
                # preceding playback receipt even when the connector published both.
                for index in (2, 3):
                    js.send({"op": "publish", "session_id": session, "revision": revision,
                             "event_id": f"t3-queue-event-{index}", "utterance_id": f"t3-queue-{index}",
                             "text": f"Queued reply {index}"})
                    assert js.event("published")["answer"]["status"] == "queued"
                    if index == 2:
                        queued = await frame(ws, "voice-speech")
                        assert queued["utterance_id"] == "t3-queue-2", queued
                try:
                    unexpected = await asyncio.wait_for(ws.recv(), 0.35)
                except TimeoutError:
                    pass
                else:
                    raise AssertionError(f"reply bypassed per-browser receipt: {unexpected}")
                assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                               body={"session_id": session, "utterance_id": "t3-queue-2",
                                     "revision": revision, "status": "playback_finished"})[0] == 200
                assert (await frame(ws, "voice-speech"))["utterance_id"] == "t3-queue-3"
                assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                               body={"session_id": session, "utterance_id": "t3-queue-3",
                                     "revision": revision, "status": "skipped"})[0] == 200
                queued_history = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
                assert [row["status"] for row in queued_history[-2:]] == ["playback_finished", "interrupted"], queued_history
                # A browser that comes to the conversation is first handed what it never heard on it.
                async with websockets.connect(f"ws://127.0.0.1:{port}/api/presentation/ws",
                                              subprotocols=["sidevoice", f"sidevoice.token.{token}"]) as returning:
                    await returning.send(json.dumps({"type": "voice-hello", "data": {}}))
                    returning_session = (await frame(returning, "voice-session"))["session_id"]
                    assert request(port, "POST", "/api/presentation/select", token=token,
                                   body={"session_id": returning_session, "thread_id": "t3-js-thread"})[0] == 200
                    replay = await frame(returning, "voice-replay")
                    assert [reply["history_id"] for reply in replay["replies"]] == [
                        f"{session}:voice:{uid}" for uid in ("t3-js-utterance", "t3-queue-2", "t3-queue-3")], replay
                    caught = await frame(returning, "voice-speech")
                    assert caught["replay"] is True and caught["history_id"] == f"{session}:voice:t3-js-utterance", caught
                async with websockets.connect(f"ws://127.0.0.1:{port}/api/presentation/ws",
                                              subprotocols=["sidevoice", f"sidevoice.token.{token}"]) as second:
                    # This listener asks for no catch-up, so the next live reply is the first it hears.
                    await second.send(json.dumps({"type": "voice-hello",
                                                  "data": {"settings": {"replay_on_return_seconds": 0}}}))
                    second_session = (await frame(second, "voice-session"))["session_id"]
                    chosen = request(port, "POST", "/api/presentation/select", token=token,
                                     body={"session_id": second_session, "thread_id": "t3-js-thread"})
                    assert chosen[0] == 200, chosen
                    second_revision = request(port, "GET", f"/api/presentation?session_id={second_session}", token=token)[1]["room"]["revision"]
                    js.send({"op": "publish", "session_id": session, "revision": revision,
                             "event_id": "t3-two-listeners-event", "utterance_id": "t3-two-listeners",
                             "text": "One reply for both listeners"})
                    assert js.event("published")["answer"]["status"] == "queued"
                    assert (await frame(ws, "voice-speech"))["utterance_id"] == "t3-two-listeners"
                    assert (await frame(second, "voice-speech"))["utterance_id"] == "t3-two-listeners"
                    assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                   body={"session_id": session, "utterance_id": "t3-two-listeners",
                                         "revision": revision, "status": "playback_finished"})[0] == 200
                    assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                   body={"session_id": second_session, "utterance_id": "t3-two-listeners",
                                         "revision": second_revision, "status": "skipped"})[0] == 200
                    two_listener_row = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"][-1]
                    assert two_listener_row["status"] == "playback_finished", two_listener_row
                # A slow host scan has its own ACK; the input receipt must arrive while it waits.
                agent_result = queue.Queue()
                threading.Thread(target=lambda: agent_result.put(request(port, "GET", "/api/host/agents?rescan=1", token=token)), daemon=True).start()
                js.method("agents.list")
                second_id = str(uuid.uuid4())
                assert request(port, "POST", "/api/presentation/text", token=token,
                               body={"text": "Input during scan", "session_id": session, "thread_id": "t3-js-thread",
                                     "binding_id": focus, "message_id": second_id})[0] == 200
                assert (await frame(ws, "voice-input-receipt", status="pending"))["thread_id"] == "t3-js-thread"
                js.method("input.deliver")
                await frame(ws, "voice-input-receipt", status="delivered", timeout=1.2)
                assert agent_result.get(timeout=4)[0] == 200
                js.send({"op": "host_error"})
                js.event("host_error_ready")
                host_error = request(port, "GET", "/api/host/agents", token=token)
                assert host_error[0] == 409 and host_error[1]["error"]["key"] == "host.agent-unavailable", host_error
                assert host_error[1]["error"]["params"] == {"agent": "fixture"}, host_error
                js.send({"op": "close"})
                js.stop()
                until(lambda: request(port, "GET", "/api/presentation/participants", token=token)[1]["participants"][0]["available"] is False)
                third_id = str(uuid.uuid4())
                assert request(port, "POST", "/api/presentation/text", token=token,
                               body={"text": "Held across reconnect", "session_id": session, "thread_id": "t3-js-thread",
                                     "binding_id": focus, "message_id": third_id})[0] == 200
                await frame(ws, "voice-input-receipt", status="pending")
                js2 = LineProcess(["node", str(Path(__file__).with_name("rust_t3_v2_peer.mjs")),
                                   str(JS_LINK), origin, ready["connector_id"], ready["token"]])
                peers.append(js2)
                js2.event("welcome")
                assert js2.event("binding")["binding"]["binding_id"] == binding["binding_id"]
                js2.method("input.deliver")
                await frame(ws, "voice-input-receipt", status="delivered")
                # Replace a peer while its ACK is delayed. Its eventual ACK cannot settle the new generation.
                js2.send({"op": "delay", "ms": 1500})
                late_id = str(uuid.uuid4())
                assert request(port, "POST", "/api/presentation/text", token=token,
                               body={"text": "Late old-generation ACK", "session_id": session, "thread_id": "t3-js-thread",
                                     "binding_id": focus, "message_id": late_id})[0] == 200
                await frame(ws, "voice-input-receipt", status="pending")
                js2.method("input.deliver")
                js3 = LineProcess(["node", str(Path(__file__).with_name("rust_t3_v2_peer.mjs")),
                                   str(JS_LINK), origin, ready["connector_id"], ready["token"]])
                peers.append(js3)
                js3.event("welcome")
                assert js3.event("binding")["binding"]["binding_id"] == binding["binding_id"]
                js3.method("input.deliver")
                await frame(ws, "voice-input-receipt", status="delivered")
                await asyncio.sleep(1.6)
                history = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
                assert next(row for row in history if row["id"].endswith(late_id))["status"] == "delivered", history
                js2.stop()
                js3.send({"op": "close"})
                js3.stop()
                until(lambda: request(port, "GET", "/api/presentation/participants", token=token)[1]["participants"][0]["available"] is False)
                # The pinned Rust proof reads Core's real ready file and opens the symmetric v3 UDS link.
                # The proof invokes a deterministic local queue command. /bin/true is on the
                # execution image, so the test does not depend on /tmp allowing executable files.
                env = {**os.environ, "SIDEVOICE_DATA_DIR": str(data), "CODEX_HOME": str(codex), "SIDEVOICE_CODEX_BIN": "/bin/true"}
                proof = subprocess.Popen([str(RUST_PEER), "connector"], env=env, stdout=subprocess.DEVNULL,
                                         stderr=subprocess.PIPE, text=True)
                peers.append(proof)
                until(lambda: (data / "connector.sock").exists() or proof.poll() is not None)
                assert proof.poll() is None, proof.stderr.read()
                ipc = ProofIPC(data / "connector.sock")
                registered = ipc.call("register", {"client_ref": "t3-rust", "harness": "codex", "thread": "t3-rust-thread",
                                                    "title": "Rust conversation", "delivery": {"kind": "codex-queue", "thread": "t3-rust-thread"}})
                assert registered["connected"] is True, registered
                chosen = request(port, "POST", "/api/presentation/select", token=token,
                                 body={"session_id": session, "thread_id": "t3-rust-thread"})
                assert chosen[0] == 200, chosen
                focus = chosen[1]["binding"]["binding_id"]
                revision = request(port, "GET", f"/api/presentation?session_id={session}", token=token)[1]["room"]["revision"]
                rust_text = request(port, "POST", "/api/presentation/text", token=token,
                                    body={"text": "Input to actual Rust v3", "session_id": session,
                                          "thread_id": "t3-rust-thread", "binding_id": focus, "message_id": str(uuid.uuid4())})
                assert rust_text[0] == 200, rust_text
                await frame(ws, "voice-input-receipt", status="pending")
                try:
                    await frame(ws, "voice-input-receipt", status="delivered")
                except AssertionError as error:
                    history = request(port, "GET", "/api/presentation/history?thread_id=t3-rust-thread", token=token)
                    proof.send_signal(signal.SIGINT)
                    proof.wait(timeout=5)
                    raise AssertionError(f"{error}; history={history}; Rust peer: {proof.stderr.read()}") from error
                stale = ipc.call("publish", {"client_ref": "t3-rust", "session_id": session,
                                             "revision": revision-1, "text": "Stale focus reply",
                                             "event_id": "t3-rust-stale-event", "utterance_id": "t3-rust-stale"})
                assert stale["status"] == "text_only" and stale["reason"] == "focus_changed", stale
                result = ipc.call("publish", {"client_ref": "t3-rust", "session_id": session, "revision": revision,
                                              "text": "Reply from actual Rust v3", "event_id": "t3-rust-event",
                                              "utterance_id": "t3-rust-utterance"})
                assert result["text_saved"] is True and result["status"] == "queued", result
                assert (await frame(ws, "voice-speech"))["text"] == "Reply from actual Rust v3"
                ipc.close()
                proof.send_signal(signal.SIGINT)
                assert proof.wait(timeout=5) == 0
                # Saturate a live v3 peer, let the callers time out, then use the
                # same socket for a fresh input and ACK. Late results stay ignored.
                v3_url = f"ws://127.0.0.1:{bridge.server_address[1]}/api/connectors/v3"
                hello = {"jsonrpc": "2.0", "id": 1, "method": "connector.hello", "params": {
                    "protocol": 3, "connector_id": ready["connector_id"], "token": ready["token"]}}
                async with websockets.connect(v3_url) as raw:
                    await raw.send(json.dumps(hello))
                    assert (await rpc_frame(raw, rid=1))["result"]["protocol"] == 3
                    await raw.send(json.dumps({"jsonrpc": "2.0", "id": 2, "method": "binding.register", "params": {
                        "client_ref": "t3-raw", "harness": "fixture", "thread": "t3-raw-thread",
                        "capabilities": {"deliver": "supported"}}}))
                    raw_binding = (await rpc_frame(raw, rid=2))["result"]["binding_id"]
                    chosen = request(port, "POST", "/api/presentation/select", token=token,
                                     body={"session_id": session, "thread_id": "t3-raw-thread"})
                    assert chosen[0] == 200, chosen
                    focus = chosen[1]["binding"]["binding_id"]
                    with concurrent.futures.ThreadPoolExecutor(max_workers=128) as pool:
                        waiting = [asyncio.wrap_future(pool.submit(request, port, "GET", "/api/host/agents",
                                                                    token=token, timeout=35)) for _ in range(128)]
                        old_ids = []
                        for _ in range(128):
                            item = await rpc_frame(raw, method="agents.list", timeout=25)
                            old_ids.append(item["id"])
                        assert len(set(old_ids)) == 128
                        expired = await asyncio.gather(*waiting)
                        assert all(status == 504 for status, _ in expired), expired
                    await raw.send(json.dumps({"jsonrpc": "2.0", "id": old_ids[0], "result": {"agents": []}}))
                    sent = request(port, "POST", "/api/presentation/text", token=token,
                                   body={"text": "Recovered on same v3 socket", "session_id": session,
                                         "thread_id": "t3-raw-thread", "binding_id": focus,
                                         "message_id": str(uuid.uuid4())})
                    assert sent[0] == 200, sent
                    await frame(ws, "voice-input-receipt", status="pending")
                    delivery = await rpc_frame(raw, method="input.deliver")
                    assert delivery["params"]["binding_id"] == raw_binding
                    await raw.send(json.dumps({"jsonrpc": "2.0", "id": delivery["id"],
                                               "result": {"status": "accepted"}}))
                    assert (await frame(ws, "voice-input-receipt", status="delivered"))["history_id"] == sent[1]["id"]
                    # The minimum signed i64 is invalid in both request and result IDs.
                    await raw.send('{"jsonrpc":"2.0","id":-9223372036854775808,"method":"input.read","params":{}}')
                    await raw.wait_closed()
                    assert raw.close_code == 1002, raw.close_code
                until(lambda: next(item for item in request(port, "GET", "/api/connectors", token=token)[1]["connectors"]
                                   if item["id"] == ready["connector_id"])["connected"] is False)
                async with websockets.connect(v3_url) as raw:
                    await raw.send(json.dumps(hello))
                    assert (await rpc_frame(raw, rid=1))["result"]["protocol"] == 3
                    await raw.send(json.dumps({"jsonrpc": "2.0", "id": 3, "method": "binding.register", "params": {
                        "client_ref": "t3-raw", "harness": "fixture", "thread": "t3-raw-thread",
                        "binding_id": raw_binding, "capabilities": {"deliver": "supported"}}}))
                    assert (await rpc_frame(raw, rid=3))["result"]["binding_id"] == raw_binding
                    sent = request(port, "POST", "/api/presentation/text", token=token,
                                   body={"text": "Delivered after malformed v3 ID", "session_id": session,
                                         "thread_id": "t3-raw-thread", "binding_id": focus,
                                         "message_id": str(uuid.uuid4())})
                    assert sent[0] == 200, sent
                    await frame(ws, "voice-input-receipt", status="pending")
                    delivery = await rpc_frame(raw, method="input.deliver")
                    await raw.send(json.dumps({"jsonrpc": "2.0", "id": delivery["id"],
                                               "result": {"status": "accepted"}}))
                    assert (await frame(ws, "voice-input-receipt", status="delivered"))["history_id"] == sent[1]["id"]
                print("T3 PASS: pinned JS v2/Rust v3, browser refusals, 128 expired v3 calls/recovery, malformed ID/reattach")
        finally:
            for peer in peers:
                if isinstance(peer, LineProcess):
                    peer.stop()
                elif peer.poll() is None:
                    peer.terminate()
                    peer.wait(timeout=5)
            if bridge:
                bridge.shutdown()
                bridge.server_close()
            if core.poll() is None:
                core.terminate()
                core.wait(timeout=10)


if __name__ == "__main__":
    asyncio.run(main())
