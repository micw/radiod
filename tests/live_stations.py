#!/usr/bin/env python3
"""Opt-in live station probe. Uses only the standard library; never reads defaults.

Only conventional block-mapping YAML is rewritten. radiod itself validates the
result. No daemon logs or MPD file fields are printed (they may contain URLs).
"""
import argparse
import datetime
import json
import math
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time
import uuid


class ProbeError(Exception):
    pass


def positive(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and positive")
    return number


def rounds_value(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def isolated_config(source, port):
    # Do not implement a partial YAML parser that might rewrite nested data.
    # Restrict the top-level syntax, leave all non-overridden bytes unchanged,
    # and let the actual runtime parse and validate the resulting document.
    if len(source.encode('utf-8')) > 1048576:
        raise ProbeError("configuration exceeds 1 MiB")
    lines = source.splitlines(keepends=True)
    seen = set()
    section = None
    result = []
    for line in lines:
        stripped = line.strip()
        if not stripped or stripped.startswith('#'):
            result.append(line)
            continue
        if line[0].isspace():
            result.append(line)
            continue
        if section == 'stations' and line.startswith('- '):
            result.append(line)  # YAML permits an indentless sequence here.
            continue
        match = re.match(r'^([A-Za-z_][A-Za-z_0-9]*):(?:\s+(.*))?\r?\n?$', line)
        if not match:
            raise ProbeError("use conventional top-level YAML block keys (no directives, flow mapping or document markers)")
        key, value = match.groups()
        section = key
        if key in seen:
            raise ProbeError("duplicate top-level YAML key")
        seen.add(key)
        if key in ('listen', 'autostart'):
            value = (value or '').strip()
            scalar = r'''(?:"(?:[^"\\]|\\.)*"|'(?:[^']|'')*'|[^\s\[\]{}&*!|>#'\"]+)(?:\s+#.*)?'''
            if not re.fullmatch(scalar, value):
                raise ProbeError("listen/autostart must be ordinary single-line YAML scalars")
            result.append(f'listen: "127.0.0.1:{port}"\n' if key == 'listen' else 'autostart: false\n')
        else:
            result.append(line)
    if result and not result[-1].endswith('\n'):
        result[-1] += '\n'
    if 'listen' not in seen:
        result.append(f'listen: "127.0.0.1:{port}"\n')
    if 'autostart' not in seen:
        result.append('autostart: false\n')
    return ''.join(result)


class MPD:
    def __init__(self, port, deadline):
        self.sock = socket.create_connection(('127.0.0.1', port), timeout=max(.001, min(1, deadline-time.monotonic())))
        self.buffer = b''
        try:
            if not self.line(deadline).startswith('OK MPD '):
                raise ProbeError('invalid MPD greeting')
        except BaseException:
            self.sock.close()
            raise

    def line(self, deadline):
        while b'\n' not in self.buffer:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProbeError('MPD response deadline exceeded')
            self.sock.settimeout(min(1, remaining))
            data = self.sock.recv(4096)
            if not data:
                raise ProbeError('MPD connection closed')
            self.buffer += data
            if len(self.buffer) > 65536:
                raise ProbeError('MPD line exceeds limit')
        line, self.buffer = self.buffer.split(b'\n', 1)
        return line.decode('utf-8', errors='replace').rstrip('\r')

    def command(self, command, deadline=None):
        deadline = min(deadline or float('inf'), time.monotonic() + 2)
        self.sock.settimeout(max(.001, deadline-time.monotonic()))
        self.sock.sendall((command + '\n').encode('ascii'))
        lines = []
        size = 0
        while True:
            line = self.line(deadline)
            if line == 'OK':
                return lines
            if line.startswith('ACK'):
                raise ProbeError('MPD command rejected')
            size += len(line)
            if size > 8 * 1048576:
                raise ProbeError('MPD response exceeds limit')
            lines.append(line)

    def status(self, deadline):
        return dict(line.split(': ', 1) for line in self.command('status', deadline) if ': ' in line)


def queue(client):
    stations = []
    current = None
    for line in client.command('playlistinfo'):
        key, _, value = line.partition(': ')
        if key == 'file':
            current = {}
            stations.append(current)
        elif key in ('Name', 'Id') and current is not None:
            current[key] = value
    if not stations:
        raise ProbeError('runtime queue is empty')
    if any(not s.get('Name') or not s.get('Id', '').isdigit() for s in stations):
        raise ProbeError('runtime queue lacks station names or numeric IDs')
    return stations


def probe(client, station, budget):
    started = time.monotonic()
    deadline = started + budget
    playing_since = None
    start_seconds = None
    client.command('stop')
    client.command('playid ' + station['Id'], deadline)
    while True:
        status = client.status(deadline)
        if status.get('radiod_state') == 'reconnecting' or 'error' in status or 'radiod_error' in status:
            raise ProbeError('runtime reported playback error/reconnecting')
        now = time.monotonic()
        if status.get('songid') != station['Id']:
            raise ProbeError('runtime changed song ID')
        if status.get('radiod_state') == 'play':
            if playing_since is None:
                playing_since = now
                start_seconds = now - started
                # The start deadline bounds first PLAYING. The subsequent
                # stability hold has its own short, bounded observation window.
                deadline = now + .75
            if now - playing_since >= .5:
                client.command('stop')
                return start_seconds
        elif playing_since is not None:
            raise ProbeError('playback did not remain playing for 0.5 seconds')
        if now >= deadline:
            raise ProbeError('playback start/hold deadline exceeded')
        time.sleep(min(.05, max(0, deadline-now)))


def checked_probe(port, station, budget):
    """Never send a command on a connection with a possibly unread response."""
    client = MPD(port, time.monotonic() + 2)
    try:
        # A fresh status response cannot be confused with a previous command.
        client.status(time.monotonic() + 2)
        return probe(client, station, budget)
    except (ProbeError, OSError):
        client.sock.close()
        # Even semantic failures invalidate the old session. Cleanup must use
        # its own greeting/response stream, not consume a late status as STOP.
        recovery = MPD(port, time.monotonic() + 2)
        try:
            recovery.command('stop')
            status = recovery.status(time.monotonic() + 2)
            if status.get('radiod_state') != 'stop':
                raise ProbeError('fresh-session cleanup did not stop playback')
        finally:
            recovery.sock.close()
        raise
    finally:
        client.sock.close()


def cleanup(process, container):
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            pass
    if container:
        # Docker CLI termination does not guarantee container termination.
        try:
            subprocess.run(['docker', 'stop', '--time', '2', container],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5, check=False)
        except (OSError, subprocess.TimeoutExpired):
            pass
    if process is not None and process.poll() is None:
        process.kill()
        process.wait(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', required=True, type=Path, help='explicit user YAML configuration; network testing is opt-in')
    parser.add_argument('--image', help='Docker image to test instead of the local binary')
    parser.add_argument('--binary', default='target/debug/radiod')
    parser.add_argument('--max-start-seconds', type=positive, default=5., help='deadline to first playing; followed by a separate 0.5 second stable-play hold (default: 5)')
    parser.add_argument('--rounds', type=rounds_value, default=2)
    parser.add_argument('--output', type=Path, help='optional JSON report path')
    parser.add_argument('--log-file', type=Path, help='optional private daemon log; never included in the JSON report')
    parser.add_argument('--trace-audio', action='store_true', help='include safe GStreamer state diagnostics in the private log')
    args = parser.parse_args()
    report = {'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'image': args.image, 'binary': None if args.image else args.binary,
              'max_start_seconds': args.max_start_seconds, 'hold_seconds': .5,
              'rounds': args.rounds, 'startup_budget_seconds': 15, 'results': []}
    process = None
    client = None
    private_log = None
    container = 'radiod-check-' + uuid.uuid4().hex if args.image else None
    try:
        source = args.config.read_text(encoding='utf-8')
        with tempfile.TemporaryDirectory(prefix='radiod-check-') as directory:
            try:
                with socket.socket() as reservation:
                    reservation.bind(('127.0.0.1', 0))
                    port = reservation.getsockname()[1]
                config = Path(directory) / 'config.yaml'
                config.write_text(isolated_config(source, port), encoding='utf-8')
                if args.image:
                    command = ['docker', 'run', '--rm', '--network', 'host', '--name', container,
                               '--read-only', '--tmpfs', '/tmp', '--tmpfs', f'/state:uid={os.getuid()},gid={os.getgid()},mode=700',
                               '--user', f'{os.getuid()}:{os.getgid()}',
                               '--mount', f'type=bind,source={directory},target=/probe,readonly',
                               '-e', 'GST_REGISTRY=/tmp/gstreamer-registry.bin', '-e', 'XDG_CACHE_HOME=/tmp/cache',
                               args.image, '--config', '/probe/config.yaml', '--state', '/state/state.json', '--null-output']
                else:
                    command = [args.binary, '--config', str(config), '--state', str(Path(directory) / 'state.json'), '--null-output']
                if args.trace_audio and args.image:
                    index = command.index(args.image)
                    command[index:index] = ['-e', 'RADIOD_TRACE_AUDIO=1']
                environment = os.environ.copy()
                if args.trace_audio:
                    environment['RADIOD_TRACE_AUDIO'] = '1'
                if args.log_file:
                    private_log = args.log_file.open('ab')
                process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=private_log or subprocess.DEVNULL, env=environment)
                deadline = time.monotonic() + 15
                while client is None:
                    if process.poll() is not None:
                        raise ProbeError('runtime exited during startup (configuration or runtime validation failed)')
                    if time.monotonic() >= deadline:
                        raise ProbeError('runtime startup deadline exceeded')
                    try:
                        client = MPD(port, deadline)
                    except OSError:
                        time.sleep(.05)
                stations = queue(client)
                client.sock.close()
                client = None
                report['station_count'] = len(stations)
                report['playback_budget_seconds'] = len(stations) * args.rounds * (args.max_start_seconds + .75)
                for round_number in range(1, args.rounds + 1):
                    for station in stations:
                        result = {'round': round_number, 'name': station['Name'], 'id': station['Id'], 'seconds': None}
                        started = time.monotonic()
                        try:
                            result['seconds'] = round(checked_probe(port, station, args.max_start_seconds), 3)
                            result['result'] = 'pass'
                        except (ProbeError, OSError) as exc:
                            result.update(result='fail', seconds=round(time.monotonic()-started, 3),
                                          error=str(exc) if isinstance(exc, ProbeError) else 'MPD transport failure')
                        report['results'].append(result)
            finally:
                if client is not None:
                    client.sock.close()
                cleanup(process, container)
                if private_log is not None:
                    private_log.close()
    except (ProbeError, OSError, UnicodeError) as exc:
        report['error'] = str(exc) if isinstance(exc, ProbeError) else 'configuration, process, or transport operation failed'
    report['result'] = 'fail' if 'error' in report or any(r['result'] == 'fail' for r in report['results']) else 'pass'
    text = json.dumps(report, ensure_ascii=True, indent=2)
    print(text)
    if args.output:
        try:
            args.output.write_text(text + '\n', encoding='utf-8')
        except OSError:
            print('Unable to write JSON report', file=__import__('sys').stderr)
            return 1
    return int(report['result'] == 'fail')


if __name__ == '__main__':
    raise SystemExit(main())
