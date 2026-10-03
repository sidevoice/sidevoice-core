"""Default-binary WebRTC peer construction on a routed, egress-filtered interface."""

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

import websockets
from aiortc import RTCPeerConnection, RTCSessionDescription


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=10)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(str(self.path))


def request(port, method, path, body, token=None, unix=None):
    connection = UnixHTTP(unix) if unix else http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    headers = {"Host": "localhost", "Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    connection.request(method, path, json.dumps(body), headers)
    response = connection.getresponse()
    result = response.status, json.loads(response.read())
    connection.close()
    return result


def restrict_egress():
    routes = subprocess.check_output(["ip", "-4", "route", "show", "scope", "link"], text=True)
    local = next((line.split()[0] for line in routes.splitlines() if " dev eth0 " in f" {line} "), None)
    assert local and "/" in local, routes
    for rule in (["-F", "OUTPUT"], ["-P", "OUTPUT", "DROP"],
                 ["-A", "OUTPUT", "-o", "lo", "-j", "ACCEPT"],
                 ["-A", "OUTPUT", "-o", "eth0", "-d", local, "-j", "ACCEPT"],
                 ["-A", "OUTPUT", "-o", "eth0", "-d", "224.0.0.251/32", "-p", "udp",
                  "--dport", "5353", "-j", "ACCEPT"]):
        subprocess.run(["iptables", *rule], check=True)
    print(json.dumps({"egress": "loopback, local bridge, mDNS only", "bridge": local}), flush=True)


def wait_ready(path, process):
    end = time.monotonic() + 40
    while time.monotonic() < end:
        if process.poll() is not None:
            raise AssertionError(f"Core exited {process.returncode}: {process.stderr.read()[-3000:]}")
        if path.exists():
            return json.loads(path.read_text())
        time.sleep(0.1)
    raise AssertionError("Core never wrote ready file")


async def frame(ws, expected):
    end = time.monotonic() + 12
    while time.monotonic() < end:
        event = json.loads(await asyncio.wait_for(ws.recv(), end - time.monotonic()))
        if event.get("type") == expected:
            return event["data"]
    raise AssertionError(f"No {expected} frame")


async def main(binary):
    restrict_egress()
    with tempfile.TemporaryDirectory(prefix="sidevoice-t7-default-rtc-") as temporary:
        data = Path(temporary) / "core"
        data.mkdir(mode=0o700)
        env = {**os.environ, "SIDEVOICE_STUN_URLS": ""}
        core = subprocess.Popen([str(binary), "--data-dir", str(data), "--port", "0",
                                 "--launch-id", "t7-default-rtc", "--idle-exit", "0"],
                                env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        peer = RTCPeerConnection()
        try:
            ready = wait_ready(data / "core.json", core)
            assert ready["launch_id"] == "t7-default-rtc"
            status, paired = request(ready["port"], "POST", "/api/device/local/pair",
                                     {"name": "T7 RTC probe"}, unix=data / "local.sock")
            assert status == 200, (status, paired)
            token = paired["token"]
            url = f"ws://127.0.0.1:{ready['port']}/api/presentation/ws"
            async with websockets.connect(url, subprotocols=["sidevoice", f"sidevoice.token.{token}"]) as ws:
                await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {}}}))
                session = (await frame(ws, "voice-session"))["session_id"]
                peer.addTransceiver("audio", direction="sendonly")
                await peer.setLocalDescription(await peer.createOffer())
                assert "a=candidate:" in peer.localDescription.sdp
                status, answer = await asyncio.to_thread(request, ready["port"], "POST",
                    "/api/presentation/rtc/offer",
                    {"session_id": session, "type": "offer", "sdp": peer.localDescription.sdp}, token)
                assert status == 200 and answer["type"] == "answer", (status, answer)
                assert "a=candidate:" in answer["sdp"], "Core answer has no gathered ICE candidate"
                await peer.setRemoteDescription(RTCSessionDescription(**answer))
                print(json.dumps({"default_mdns": True, "gathered_offer_answer": True,
                                  "core_launch_id": ready["launch_id"],
                                  "candidate_count": answer["sdp"].count("a=candidate:")}), flush=True)
        finally:
            await peer.close()
            core.send_signal(signal.SIGTERM)
            assert core.wait(timeout=15) == 0, core.stderr.read()[-3000:]
            assert not (data / "core.json").exists()


if __name__ == "__main__":
    asyncio.run(main(Path(sys.argv[1]).resolve()))
