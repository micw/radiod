#!/usr/bin/env python3
"""Offline, real-HTTP indefinite AAC/ICY regression; Python stdlib only.

Run after cargo build, or set RADIOD_BINARY. All sockets use 127.0.0.1.
A 60-second watchdog includes bounded child/server cleanup. Startup and each
restart/switch must reach actual radiod_state=play with matching ICY tags in 5s.
"""
import collections
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("RADIOD_BINARY", str(ROOT / "target/debug/radiod"))).resolve()
METAINT = 8192
TITLES = {"/realtime.aac": "Synthetic Realtime", "/bursts.aac": "Synthetic Bursts"}
DEADLINE = 0.0
STOP = threading.Event()


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def remaining(cap=1.0):
    value = min(cap, DEADLINE - time.monotonic())
    if value <= 0:
        raise TimeoutError("streaming regression deadline exceeded")
    return value


def eventually(predicate, message, timeout=5.0):
    end = min(DEADLINE, time.monotonic() + timeout)
    while True:
        if predicate():
            return
        if time.monotonic() >= end:
            raise AssertionError(message)
        STOP.wait(min(0.025, end - time.monotonic()))


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

    def command(self, command):
        self.sock.settimeout(remaining())
        self.sock.sendall((command + "\n").encode())
        lines = []
        while True:
            line = self.line()
            if line == "OK":
                return lines
            check(not line.startswith("ACK "), "MPD rejected command: " + line)
            lines.append(line)

    def fields(self, command):
        return dict(line.split(": ", 1) for line in self.command(command) if ": " in line)

    def close(self):
        self.file.close()
        self.sock.close()


def main():
    global DEADLINE
    started = time.monotonic()
    DEADLINE = started + 54  # Reserve six seconds of watchdog for cleanup.
    check(BINARY.is_file(), "radiod binary missing; run cargo build or set RADIOD_BINARY")
    audio = (ROOT / "tests/fixtures/live.aac").read_bytes()
    check(0 < len(audio) <= 40 * 1024, "AAC fixture must be nonempty and <=40 KiB")
    # Validate frame boundaries once: repeating the fixture must not splice ADTS frames.
    offset = 0
    frames = 0
    while offset < len(audio):
        header = audio[offset:offset + 7]
        check(len(header) == 7 and header[0] == 0xff and header[1] & 0xf6 == 0xf0,
              "invalid ADTS fixture sync/header")
        size = ((header[3] & 3) << 11) | (header[4] << 3) | (header[5] >> 5)
        check(size >= 7 and offset + size <= len(audio), "truncated ADTS fixture frame")
        offset += size
        frames += 1
    rate = len(audio) / (frames * 1024 / 48000)  # Encoded bytes per second.
    lock = threading.Lock()
    counts = collections.Counter()
    active = set()
    serial = 0
    server_errors = []

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self):
            nonlocal serial
            self.connection.settimeout(1.0)
            if self.path not in TITLES:
                self.send_error(404)
                return
            with lock:
                serial += 1
                token = serial
                counts[self.path] += 1
                active.add(token)
            try:
                self.send_response(200)
                self.send_header("Content-Type", "audio/aac")
                self.send_header("Connection", "close")
                self.send_header("icy-name", "Synthetic local regression")
                self.send_header("icy-br", "96")
                self.send_header("icy-metaint", str(METAINT))
                self.end_headers()
                title = ("StreamTitle='%s';" % TITLES[self.path]).encode("ascii")
                padded = title + b"\0" * (-len(title) % 16)
                metadata = bytes([len(padded) // 16]) + padded
                position = 0
                until_metadata = METAINT

                def send(amount):
                    nonlocal position, until_metadata
                    while amount and not STOP.is_set():
                        size = min(amount, len(audio) - position, until_metadata)
                        self.wfile.write(audio[position:position + size])
                        position = (position + size) % len(audio)
                        amount -= size
                        until_metadata -= size
                        if until_metadata == 0:
                            self.wfile.write(metadata)
                            until_metadata = METAINT
                    self.wfile.flush()

                # Fast initial preroll, then indefinite realtime or one-second bursts.
                send(4 * METAINT)
                interval = 1.0 if self.path == "/bursts.aac" else 0.1
                amount = max(1, round(rate * interval))
                while not STOP.wait(interval):
                    send(amount)
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, socket.timeout):
                pass  # Cancellation and slow/closed clients are expected.
            except Exception as error:
                with lock:
                    server_errors.append(repr(error))
            finally:
                self.close_connection = True
                with lock:
                    active.discard(token)

        def log_message(self, *_args):
            pass

    def active_count():
        with lock:
            return len(active)

    def request_count():
        with lock:
            return sum(counts.values())

    process = client = server = thread = None

    def watchdog(_signum, _frame):
        raise TimeoutError("60-second streaming watchdog expired")

    old_handler = signal.signal(signal.SIGALRM, watchdog)
    signal.setitimer(signal.ITIMER_REAL, 60)
    try:
        with tempfile.TemporaryDirectory(prefix="radiod-streaming-") as directory:
            directory = Path(directory)
            log_path = directory / "daemon.log"
            try:
                server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
                server.daemon_threads = True
                thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.05), daemon=True)
                thread.start()
                with socket.socket() as reservation:
                    reservation.bind(("127.0.0.1", 0))
                    port = reservation.getsockname()[1]
                base = "http://127.0.0.1:%d" % server.server_port
                config = directory / "radio.yaml"
                config.write_text(
                    "listen: " + json.dumps("127.0.0.1:%d" % port) + "\n"
                    "initial_volume: 20\nmax_volume: 45\nretry_max_seconds: 1\n"
                    "connect_timeout_seconds: 5\nautostart: false\nstations:\n" +
                    "".join("  - name: %s\n    url: %s\n" % (json.dumps(title), json.dumps(base + path))
                            for path, title in TITLES.items()), encoding="utf-8")
                with log_path.open("wb") as log:
                    process = subprocess.Popen([str(BINARY), "--config", str(config), "--state",
                                                str(directory / "state.json"), "--null-output"],
                                               cwd=directory, stdout=log, stderr=log)

                def connect():
                    nonlocal client
                    check(process.poll() is None, "radiod exited during startup")
                    try:
                        client = MPD(port)
                        return True
                    except (ConnectionRefusedError, TimeoutError):
                        return False

                eventually(connect, "MPD did not start within five seconds")
                check(client.fields("status").get("radiod_state") == "stop", "startup was not stopped")
                client.command('load "radio"')
                entries = []
                for line in client.command("playlistinfo"):
                    if line.startswith("file: "):
                        entries.append({})
                    if ": " in line and entries:
                        key, value = line.split(": ", 1)
                        entries[-1][key] = value
                by_path = {path: next(row for row in entries if row.get("file") == base + path)
                           for path in TITLES}
                last_status = {}

                def playing(path):
                    nonlocal last_status
                    check(process.poll() is None, "radiod exited during playback")
                    last_status = client.fields("status")
                    song = client.fields("currentsong")
                    return (last_status.get("radiod_state") == "play"
                            and "radiod_error" not in last_status
                            and last_status.get("songid") == by_path[path]["Id"]
                            and song.get("Title") == TITLES[path])

                def start(path, label):
                    before = request_count()
                    begin = time.monotonic()
                    client.command("playid " + by_path[path]["Id"])
                    eventually(lambda: playing(path) and request_count() > before,
                               label + " did not reach play with ICY title in five seconds")
                    check(time.monotonic() - begin <= 5.0, label + " exceeded five seconds")

                def stop():
                    client.command("stop")
                    check(client.fields("status").get("radiod_state") == "stop", "stop did not cancel playback")
                    eventually(lambda: active_count() == 0, "stop left a streaming HTTP client active", 2.5)
                    before = request_count()
                    STOP.wait(0.15)
                    check(request_count() == before, "stop allowed a reconnect")
                    check(client.fields("status").get("radiod_state") == "stop", "stopped state changed")

                start("/realtime.aac", "initial AAC startup")
                # Observe each delivery cadence beyond its initial preroll burst.
                for path in TITLES:
                    if path != "/realtime.aac":
                        start(path, "burst startup")
                    end = time.monotonic() + 1.25
                    while time.monotonic() < end:
                        # A source/decoder may legitimately refill after its
                        # initial fast burst. Require bounded recovery without
                        # reconnecting or selecting another stream, not a
                        # permanent PLAYING status that hides real buffering.
                        if not playing(path):
                            before = request_count()
                            eventually(lambda: playing(path), "stream did not recover from refill: " + repr(last_status), 3.0)
                            check(request_count() == before, "refill unexpectedly reconnected the stream")
                        STOP.wait(0.05)
                for index in range(5):
                    stop()
                    start("/realtime.aac", "restart %d" % (index + 1))
                for index in range(5):
                    path = "/bursts.aac" if index % 2 == 0 else "/realtime.aac"
                    start(path, "switch %d" % (index + 1))
                stop()
                with lock:
                    check(not server_errors, "HTTP server errors: " + repr(server_errors))
                print("streaming OK: AAC/ICY startup, realtime + bursts, 5 restarts, 5 switches, "
                      "stop cancellation; %.2fs, %d HTTP requests, %d-byte fixture" %
                      (time.monotonic() - started, request_count(), len(audio)))
            except BaseException:
                if log_path.exists():
                    print("--- radiod log ---\n" + log_path.read_text(errors="replace"), flush=True)
                raise
            finally:
                STOP.set()
                if client is not None:
                    client.close()
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)
                if server is not None:
                    server.shutdown()
                    server.server_close()
                if thread is not None:
                    thread.join(timeout=1)
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old_handler)


if __name__ == "__main__":
    main()
