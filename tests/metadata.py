#!/usr/bin/env python3
"""Offline real-binary ICY -> MPD playlist-cache regression (stdlib only).

Run after cargo build, or set RADIOD_BINARY. Uses the unchanged live.aac fixture,
loopback HTTP and --null-output; a 40-second watchdog includes cleanup.
"""
import collections
import http.server
import json
import signal
import socket
import subprocess
import tempfile
import threading
import time
from pathlib import Path

# Share the streaming regression's bounded MPD client and fixture conventions.
import streaming as common


def rows(lines):
    result = []
    for line in lines:
        if line.startswith("file: "):
            result.append({})
        if result and ": " in line:
            key, value = line.split(": ", 1)
            result[-1][key] = value
    return result


def main():
    started = time.monotonic()
    common.DEADLINE = started + 34  # Reserve six seconds for bounded cleanup.
    common.STOP.clear()
    check = common.check
    eventually = common.eventually
    check(common.BINARY.is_file(), "radiod binary missing; build or set RADIOD_BINARY")
    audio = (common.ROOT / "tests/fixtures/live.aac").read_bytes()
    check(bool(audio), "empty AAC fixture")
    offset = frames = 0
    while offset < len(audio):
        header = audio[offset:offset + 7]
        check(len(header) == 7 and header[0] == 0xff and header[1] & 0xf6 == 0xf0,
              "invalid ADTS fixture sync/header")
        size = ((header[3] & 3) << 11) | (header[4] << 3) | (header[5] >> 5)
        check(size >= 7 and offset + size <= len(audio), "truncated ADTS fixture frame")
        offset += size
        frames += 1
    rate = len(audio) / (frames * 1024 / 48000)
    block_interval = 8192 / rate
    lock = threading.Lock()
    titles = {"/one.aac": "Title A", "/two.aac": "Second title"}
    names = {"/one.aac": "Station One", "/two.aac": "Station Two"}
    gates = {path: threading.Event() for path in names}
    for gate in gates.values():
        gate.set()
    blocks = collections.Counter()
    requests = collections.Counter()
    active = set()
    errors = []

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self):
            if self.path not in names:
                self.send_error(404)
                return
            token = object()
            self.connection.settimeout(0.5)
            with lock:
                requests[self.path] += 1
                active.add(token)
            try:
                # Gate headers as well as audio: no new source metadata can race
                # the reset assertions while the second request is held here.
                while not gates[self.path].wait(0.025):
                    if common.STOP.is_set():
                        return
                self.send_response(200)
                self.send_header("Content-Type", "audio/aac")
                self.send_header("Connection", "close")
                self.send_header("icy-metaint", "8192")
                self.end_headers()
                position = 0
                while not common.STOP.is_set():
                    with lock:
                        title = titles[self.path]
                    payload = ("StreamTitle='%s';" % title).encode("ascii")
                    payload += b"\0" * (-len(payload) % 16)
                    amount = 8192
                    while amount:
                        size = min(amount, len(audio) - position)
                        self.wfile.write(audio[position:position + size])
                        position = (position + size) % len(audio)
                        amount -= size
                    self.wfile.write(bytes([len(payload) // 16]) + payload)
                    self.wfile.flush()
                    with lock:
                        blocks[(self.path, title)] += 1
                    # One immediate block is less than one second of preroll;
                    # thereafter deliver audio at the fixture's encoded rate.
                    if common.STOP.wait(block_interval):
                        return
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, socket.timeout):
                pass
            except Exception as error:
                with lock:
                    errors.append(repr(error))
            finally:
                self.close_connection = True
                with lock:
                    active.discard(token)

        def log_message(self, *_args):
            pass

    def count(counter, key):
        with lock:
            return counter[key]

    def watchdog(_signum, _frame):
        raise TimeoutError("40-second metadata watchdog expired")

    previous = signal.signal(signal.SIGALRM, watchdog)
    signal.setitimer(signal.ITIMER_REAL, 40)
    process = client = idle = server = thread = None
    try:
        with tempfile.TemporaryDirectory(prefix="radiod-metadata-") as temporary:
            directory = Path(temporary)
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
                    "initial_volume: 20\nretry_max_seconds: 1\n"
                    "connect_timeout_seconds: 5\nautostart: false\nstations:\n" +
                    "".join("  - name: %s\n    url: %s\n" %
                            (json.dumps(name), json.dumps(base + path)) for path, name in names.items()),
                    encoding="utf-8")
                with log_path.open("wb") as log:
                    process = subprocess.Popen([str(common.BINARY), "--config", str(config),
                                                "--state", str(directory / "state.json"), "--null-output"],
                                               cwd=directory, stdout=log, stderr=log)

                def connect():
                    nonlocal client
                    check(process.poll() is None, "radiod exited during startup")
                    try:
                        client = common.MPD(port)
                        return True
                    except (ConnectionRefusedError, TimeoutError):
                        return False

                eventually(connect, "MPD startup timed out")
                client.command('load "radio"')
                original = rows(client.command("playlistinfo"))
                check(len(original) == 2, "expected two configured stations")
                ids = {path: next(row["Id"] for row in original if row["file"] == base + path)
                       for path in names}
                shape = [(row["file"], row["Id"], row["Pos"]) for row in original]

                def version():
                    return int(client.fields("status")["playlist"])

                def title_is(title):
                    return client.fields("currentsong").get("Title") == title

                def changed_since(old):
                    return rows(client.command("plchanges %d" % old))

                def assert_shape():
                    check([(r["file"], r["Id"], r["Pos"]) for r in rows(client.command("playlistinfo"))]
                          == shape, "metadata changed playlist order, IDs, URLs or positions")

                client.command("playid " + ids["/one.aac"])
                eventually(lambda: title_is("Title A"), "initial ICY Title A not received")
                old = version()
                cache = {row["Id"]: row for row in rows(client.command("playlistinfo"))}
                check(cache[ids["/one.aac"]].get("Title") == "Title A", "initial playlist title missing")
                idle = common.MPD(port)
                idle.sock.sendall(b"idle playlist\n")
                with lock:
                    titles["/one.aac"] = "Title B"
                # Read the idle response with a bounded wait, not by issuing a
                # second command on the idle connection.
                notifications = []
                while True:
                    idle.sock.settimeout(common.remaining(5.0))
                    raw = idle.file.readline(65537)
                    check(raw and len(raw) <= 65536, "closed/oversized idle response")
                    line = raw.decode("utf-8").rstrip("\r\n")
                    if line == "OK":
                        break
                    check(not line.startswith("ACK "), "idle failed: " + line)
                    notifications.append(line)
                check("changed: playlist" in notifications, "ICY change did not notify idle playlist")
                eventually(lambda: title_is("Title B"), "currentsong did not expose Title B")
                check(version() > old, "changed metadata did not increase playlist version")
                changes = changed_since(old)
                check(any(r.get("Id") == ids["/one.aac"] and r.get("Title") == "Title B" for r in changes),
                      "plchanges omitted current ID/new Title B")
                cache.update((row["Id"], row) for row in changes)
                check(cache[ids["/one.aac"]].get("Title") == "Title B", "MPDroid-style cache stayed stale")
                check(count(requests, "/one.aac") == 1, "title change reconnected source")
                assert_shape()

                stable = version()
                sent = count(blocks, ("/one.aac", "Title B"))
                # Observe actual repeated blocks, checking throughout and after
                # delivery to catch a version storm (not an arbitrary sleep).
                def repeated():
                    check(version() == stable, "identical ICY metadata increased playlist version")
                    return count(blocks, ("/one.aac", "Title B")) >= sent + 8
                eventually(repeated, "server did not send repeated Title B blocks", 8)
                end = time.monotonic() + 0.35
                while time.monotonic() < end:
                    check(version() == stable, "identical metadata caused a delayed version storm")
                    common.STOP.wait(0.025)

                old = version()
                client.command("playid " + ids["/two.aac"])
                eventually(lambda: title_is("Second title"), "station switch metadata missing")
                check(version() > old, "station switch failed to invalidate cached metadata")
                changes = {row["Id"]: row for row in changed_since(old)}
                check(changes.get(ids["/one.aac"], {}).get("Title") == names["/one.aac"],
                      "former station retained Title B instead of fallback station name")
                check(changes.get(ids["/two.aac"], {}).get("Title") == "Second title",
                      "plchanges omitted new station metadata")
                assert_shape()

                # Reset the SAME station; hold the new HTTP response before its
                # headers so the reset is observable before any fresh ICY tags.
                gates["/two.aac"].clear()
                before_requests = count(requests, "/two.aac")
                old = version()
                client.command("playid " + ids["/two.aac"])
                eventually(lambda: count(requests, "/two.aac") > before_requests,
                           "same-station playid did not reset source")
                check(version() > old, "source reset did not bump playlist version before metadata")
                check(client.fields("currentsong").get("Title") == names["/two.aac"],
                      "source reset retained the old current title")
                reset_changes = {row["Id"]: row for row in changed_since(old)}
                check(reset_changes.get(ids["/two.aac"], {}).get("Title") == names["/two.aac"],
                      "reset plchanges did not clear cached current title")
                reset_version = version()
                with lock:
                    titles["/two.aac"] = "After reset"
                gates["/two.aac"].set()
                eventually(lambda: title_is("After reset"), "delayed reset metadata not received")
                check(version() > reset_version, "new metadata after reset did not bump version")
                assert_shape()
                # No Artist assertion: this synthetic fixture only supplies StreamTitle.
                client.command("stop")
                check(client.fields("status").get("radiod_state") == "stop", "terminal stop failed")
                def inactive():
                    with lock:
                        return not active
                eventually(inactive, "terminal stop left HTTP sources active", 2.5)
                with lock:
                    check(not errors, "HTTP server errors: " + repr(errors))
                print("metadata OK: idle, plchanges cache, identical tags, switch and source reset; %.2fs" %
                      (time.monotonic() - started))
            except BaseException:
                if log_path.exists():
                    print("--- radiod log ---\n" + log_path.read_text(errors="replace"), flush=True)
                raise
            finally:
                common.STOP.set()
                for gate in gates.values():
                    gate.set()
                for connection in (idle, client):
                    if connection is not None:
                        connection.close()
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
        signal.signal(signal.SIGALRM, previous)


if __name__ == "__main__":
    main()
