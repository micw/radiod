#!/usr/bin/env python3
"""Offline integration smoke test. Run after cargo build: python3 tests/smoke.py.

Only Python's standard library is required. All HTTP/MPD traffic stays on
127.0.0.1; --null-output prevents access to real audio devices. A 28-second
watchdog bounds the test, with a further bounded subprocess cleanup.
"""
import collections
import http.server
import io
import json
import math
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import wave


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("RADIOD_BINARY", ROOT / "target/debug/radiod"))
DEADLINE = 0.0
PAUSE = threading.Event()


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def remaining(cap=2.0):
    value = min(cap, DEADLINE - time.monotonic())
    if value <= 0:
        raise TimeoutError("smoke test exceeded its deadline")
    return value


def eventually(predicate, message, timeout=4.0):
    end = min(DEADLINE, time.monotonic() + timeout)
    while True:
        if predicate():
            return
        if time.monotonic() >= end:
            raise AssertionError(message)
        PAUSE.wait(min(0.05, end - time.monotonic()))


class MPD:
    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port), remaining())
        self.file = self.sock.makefile("rb")
        check(self.line().startswith("OK MPD "), "missing MPD greeting")

    def line(self):
        self.sock.settimeout(remaining())
        line = self.file.readline(65537)
        check(line and len(line) <= 65536, "closed/oversized MPD response")
        return line.decode("utf-8").rstrip("\r\n")

    def send(self, text):
        self.sock.settimeout(remaining())
        self.sock.sendall((text + "\n").encode())

    def response(self):
        lines = []
        while True:
            line = self.line()
            if line == "OK":
                return lines
            if line.startswith("ACK "):
                raise AssertionError("MPD rejected command: " + line)
            lines.append(line)

    def command(self, text):
        self.send(text)
        return self.response()

    def status(self):
        return dict(line.split(": ", 1) for line in self.command("status") if ": " in line)

    def close(self):
        self.file.close()
        self.sock.close()


def songs(lines):
    result = []
    for line in lines:
        if ": " not in line:
            continue
        key, value = line.split(": ", 1)
        if key == "file":
            result.append({})
        if result:
            result[-1][key] = value
    return result


def main():
    global DEADLINE
    if not BINARY.is_file():
        raise RuntimeError("target/debug/radiod is unavailable; run cargo build first")
    DEADLINE = time.monotonic() + 27
    process = client = server = thread = None
    counts = collections.Counter()
    lock = threading.Lock()
    wav = io.BytesIO()
    with wave.open(wav, "wb") as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(8000)
        out.writeframes(b"".join(struct.pack("<h", int(2000 * math.sin(i * math.tau * 440 / 8000))) for i in range(1600)))
    audio = wav.getvalue()

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            with lock:
                counts[self.path] += 1
            payload = audio if self.path == "/tone.wav" else b"deliberate failure"
            self.send_response(200 if self.path == "/tone.wav" else 503)
            self.send_header("Content-Type", "audio/wav" if self.path == "/tone.wav" else "text/plain")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            try:
                self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def log_message(self, *_args):
            pass

    def count(path):
        with lock:
            return counts[path]

    def watchdog(_signum, _frame):
        raise TimeoutError("28-second smoke watchdog expired")

    old_handler = signal.signal(signal.SIGALRM, watchdog)
    signal.setitimer(signal.ITIMER_REAL, 28)
    try:
        with tempfile.TemporaryDirectory(prefix="radiod-smoke-") as directory:
            directory = Path(directory)
            config = directory / "radio.yaml"
            log_path = directory / "daemon.log"
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            server.daemon_threads = True
            thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.05), daemon=True)
            thread.start()
            base = "http://127.0.0.1:%d" % server.server_port
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]

            def replace(stations, invalid=False):
                text = "listen: " + json.dumps("127.0.0.1:%d" % port) + "\n"
                text += "initial_volume: 20\nretry_max_seconds: 1\nconnect_timeout_seconds: 2\nautostart: false\n"
                text += "stations:\n"
                for name, url in stations:
                    text += "  - name: %s\n    url: %s\n" % (json.dumps(name), json.dumps(url))
                if invalid:
                    text += "unknown_smoke_option: true\n"
                pending = directory / "replacement.yaml"
                pending.write_text(text, encoding="utf-8")
                os.replace(pending, config)

            initial = [("Tone", base + "/tone.wav"), ("Failure", base + "/failure")]
            replace(initial)
            with log_path.open("wb") as log:
                process = subprocess.Popen([str(BINARY), "--config", str(config), "--state", str(directory / "state.json"), "--null-output"], cwd=directory, stdout=log, stderr=log)
                try:
                    def connect():
                        nonlocal client
                        check(process.poll() is None, "radiod exited during startup")
                        try:
                            client = MPD(port)
                            return True
                        except (ConnectionRefusedError, TimeoutError):
                            return False
                    eventually(connect, "MPD did not become ready", 5)
                    check(client.status().get("state") == "stop", "expected stopped startup")
                    playlists = client.command("listplaylists")
                    names = [line.split(": ", 1)[1] for line in playlists if line.startswith("playlist: ")]
                    check(names, "no stored station playlist")
                    client.command("load " + json.dumps(names[0]))
                    entries = songs(client.command("playlistinfo"))
                    by_url = {entry["file"]: entry for entry in entries}
                    check(set(by_url) == {url for _, url in initial}, "station playlist mismatch")
                    tone_id = by_url[base + "/tone.wav"]["Id"]
                    failure_id = by_url[base + "/failure"]["Id"]
                    client.command("setvol 100")
                    check(client.status().get("volume") == "100", "full protocol volume range unavailable")
                    client.command("play " + by_url[base + "/tone.wav"]["Pos"])
                    eventually(lambda: count("/tone.wav") >= 2, "WAV EOS did not trigger retry", 5)
                    check(client.status().get("songid") == tone_id, "EOS changed current ID")
                    client.command("stop")
                    check(client.status().get("state") == "stop", "stop failed")
                    client.command("playid " + failure_id)
                    eventually(lambda: count("/failure") >= 2, "HTTP failure did not reconnect", 5)
                    check(client.status().get("songid") == failure_id, "retry changed current ID")
                    client.close()
                    client = MPD(port)
                    check(client.status().get("songid") == failure_id, "client reconnect lost current ID")
                    renamed = [("Renamed failure", base + "/failure"), ("Renamed tone", base + "/tone.wav")]
                    replace(renamed)
                    def reloaded():
                        rows = songs(client.command("playlistinfo"))
                        return len(rows) == 2 and rows[0].get("Id") == failure_id and rows[0].get("Name") == "Renamed failure" and rows[1].get("Id") == tone_id and client.status().get("volume") == "100"
                    eventually(reloaded, "atomic rename/reorder reload changed volume or failed")
                    replace([("Invalid replacement", base + "/other")], invalid=True)
                    # Wait for the actual watcher rejection, not an arbitrary sleep.
                    eventually(lambda: "configuration rejected" in log_path.read_text(errors="replace"), "invalid configuration was not rejected")
                    check(reloaded(), "invalid configuration replaced the previous playlist")
                    replace([("Renamed tone", base + "/tone.wav")])
                    eventually(lambda: len(songs(client.command("playlistinfo"))) == 1 and client.status().get("state") == "stop", "removing active URL did not stop playback")
                    # Standard MPD command lists must preserve response framing.
                    client.send("command_list_ok_begin\nstatus\nplaylistinfo\ncommand_list_end")
                    check(client.response().count("list_OK") == 2, "command-list framing mismatch")
                    supported = client.command("commands")
                    if "command: idle" in supported and "command: noidle" in supported:
                        other = MPD(port)
                        try:
                            other.command("setvol 15")
                        finally:
                            other.close()
                        # Change happened before entry into idle: it must not be lost.
                        client.send("idle mixer")
                        check("changed: mixer" in client.response(), "pre-idle change was lost")
                        # Cancellation can race with an already delivered notification.
                        # A stray noidle must not add an extra OK to the next response.
                        client.send("noidle\nstatus")
                        check(any(line.startswith("state: ") for line in client.response()), "stray noidle desynchronized protocol")
                        client.send("idle player")
                        client.send("noidle")
                        client.response()
                        client.command("ping")  # No orphaned idle response may remain.
                    else:
                        print("SKIP: idle/noidle not advertised")
                    client.command("stop")
                    check(client.status().get("state") == "stop", "final stop failed")
                    check(process.poll() is None, "radiod exited unexpectedly")
                    client.close()
                    client = None
                    process.terminate()
                    check(process.wait(timeout=remaining()) in (0, -signal.SIGTERM), "unexpected subprocess termination status")
                    print("PASS: MPD, local WAV EOS/HTTP retry, hot reload, IDs, volume and cleanup")
                except BaseException:
                    print(log_path.read_text(errors="replace"))
                    raise
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old_handler)
        if client is not None:
            client.close()
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=1)
        if server is not None:
            server.shutdown()
            server.server_close()
        if thread is not None:
            thread.join(timeout=0.2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
