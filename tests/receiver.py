#!/usr/bin/env python3
"""Offline actual-binary receiver regression (stdlib; null output ONLY).

Run after building radiod, or set RADIOD_BINARY. No hardware, firmware, network
services or Python packages are required. A 70 second watchdog includes cleanup.
AAC decoding is mandatory: missing runtime codecs fail rather than skip this test.
"""
import base64
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('RADIOD_BINARY', ROOT / 'target/debug/radiod')).resolve()
STOP = threading.Event()
DEADLINE = 0


def check(value, message):
    if not value:
        raise AssertionError(message)


def remaining(cap=1):
    left = min(cap, DEADLINE - time.monotonic())
    if left <= 0:
        raise TimeoutError('receiver regression deadline exceeded')
    return left


def eventually(test, message, timeout=5):
    end = min(DEADLINE, time.monotonic() + timeout)
    while not test():
        check(time.monotonic() < end, message)
        STOP.wait(.025)


def freeport(udp=False):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM if udp else socket.SOCK_STREAM) as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


class MPD:
    def __init__(self, port, host='127.0.0.1'):
        self.sock = socket.create_connection((host, port), remaining())
        self.file = self.sock.makefile('rb')
        check(self.line().startswith('OK MPD '), 'MPD greeting')

    def line(self):
        self.sock.settimeout(remaining())
        line = self.file.readline(65537)
        check(line and len(line) <= 65536, 'closed/oversized MPD response')
        return line.decode().rstrip('\r\n')

    def command(self, command):
        self.sock.sendall((command + '\n').encode())
        lines = []
        while True:
            line = self.line()
            if line == 'OK':
                return lines
            check(not line.startswith('ACK '), 'MPD command rejected: ' + line)
            lines.append(line)

    def fields(self, command):
        return dict(line.split(': ', 1) for line in self.command(command) if ': ' in line)

    def rows(self, command='playlistinfo'):
        rows = []
        for line in self.command(command):
            if line.startswith('file: '):
                rows.append({})
            if rows and ': ' in line:
                key, value = line.split(': ', 1)
                rows[-1][key] = value
        return rows

    def close(self):
        self.file.close()
        self.sock.close()


class WS:
    """Small RFC6455 client: validated handshake, masked writes, bounded frames."""
    def __init__(self, port, device='receiver', absolute=True, rejected=False, legacy=False, host='127.0.0.1'):
        self.device = device
        self.legacy = legacy
        self.sock = socket.create_connection((host, port), remaining())
        self.buffer = bytearray()
        self.messages = []
        self.seq = 1
        self.server_seq = 0
        self.session = None
        self.last_heartbeat = time.monotonic()
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(('GET /v1/receivers HTTP/1.1\r\nHost: 127.0.0.1\r\n'
                           'Upgrade: websocket\r\nConnection: Upgrade\r\n'
                           'Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: ' + key + '\r\n'
                           'Sec-WebSocket-Protocol: radiod.receiver.v1\r\n\r\n').encode())
        while b'\r\n\r\n' not in self.buffer:
            self.sock.settimeout(remaining())
            data = self.sock.recv(4096)
            check(data, 'closed during WS handshake')
            self.buffer.extend(data)
            check(len(self.buffer) <= 16384, 'oversized WS handshake')
        head, tail = bytes(self.buffer).split(b'\r\n\r\n', 1)
        self.buffer = bytearray(tail)
        lines = head.decode().split('\r\n')
        headers = dict((k.lower(), v.strip()) for k, v in (x.split(':', 1) for x in lines[1:]))
        check(lines[0].startswith('HTTP/1.1 101 '), 'WS upgrade rejected')
        check(headers.get('upgrade', '').lower() == 'websocket' and
              'upgrade' in headers.get('connection', '').lower().split(', '), 'invalid WS upgrade')
        expected = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
        check(headers.get('sec-websocket-accept') == expected, 'invalid WS accept')
        check(headers.get('sec-websocket-protocol') == 'radiod.receiver.v1', 'wrong subprotocol')
        hello = dict(v=1, type='hello', seq=1, device_id=device, boot_id='boot')
        if legacy:
            # Exact deployed-firmware dialect: no firmware/top-level codecs fields.
            hello.update(name='Synthetic ESP receiver', capabilities=dict(
                         codecs=['sbc', 'aac-lc'], transport_control=True, absolute_volume=absolute))
        else:
            hello.update(firmware='offline-test', capabilities=dict(play=True, pause=True,
                         absolute_volume=absolute, metadata=True), codecs=['aac-lc'])
        self.raw(hello)
        if rejected:
            self.wait('error')
            return
        self.welcome = self.wait('welcome')
        self.session = self.welcome['control_session_id']

    def frame(self, payload, opcode=1):
        check(len(payload) <= 16384, 'client frame cap')
        mask = os.urandom(4)
        header = bytes([0x80 | opcode, 0x80 | (len(payload) if len(payload) < 126 else 126)])
        if len(payload) >= 126:
            header += struct.pack('!H', len(payload))
        self.sock.sendall(header + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def raw(self, message):
        self.frame(json.dumps(message, separators=(',', ':')).encode())

    def send(self, kind, **fields):
        self.seq += 1
        self.raw(dict(v=1, type=kind, seq=self.seq, boot_id='boot', device_id=self.device,
                      control_session_id=self.session, **fields))

    def exact(self, count):
        while len(self.buffer) < count:
            self.sock.settimeout(remaining(.15))
            data = self.sock.recv(16384)
            if not data:
                raise EOFError('WS closed')
            self.buffer.extend(data)
        data = bytes(self.buffer[:count])
        del self.buffer[:count]
        return data

    def receive(self):
        a, b = self.exact(2)
        check(a & 0x80 and not a & 0x70 and not b & 0x80, 'unsupported server WS frame')
        size = b & 127
        if size == 126:
            size = struct.unpack('!H', self.exact(2))[0]
        elif size == 127:
            size = struct.unpack('!Q', self.exact(8))[0]
        check(size <= 16384, 'server frame cap')
        payload = self.exact(size)
        if a & 15 == 8:
            raise EOFError('WS closed')
        if a & 15 == 9:
            self.frame(payload, 10)
            return None
        check(a & 15 in (1, 10), 'unexpected server opcode')
        if a & 15 != 1:
            return None
        value = json.loads(payload)
        check(value.get('v') == 1, 'wrong server protocol version')
        if value.get('type') != 'error':
            check(value.get('boot_id') == 'boot' and isinstance(value.get('seq'), int) and
                  value['seq'] > self.server_seq, 'invalid server sequence/boot envelope')
            self.server_seq = value['seq']
            if self.session:
                check(value.get('control_session_id') == self.session, 'wrong server session envelope')
        return value

    def pump(self):
        if self.session and time.monotonic() - self.last_heartbeat > .4:
            self.send('heartbeat')
            self.last_heartbeat = time.monotonic()
        try:
            value = self.receive()
            if value:
                self.messages.append(value)
        except socket.timeout:
            pass

    def wait(self, kind, predicate=lambda m: True, timeout=4):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            for i, m in enumerate(self.messages):
                if m.get('type') == kind and predicate(m):
                    return self.messages.pop(i)
            self.pump()
        raise AssertionError('missing receiver message: ' + kind)

    def hold(self, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.pump()

    def snapshot(self, name='Actual phone', connected=True, started=False, **extra):
        peer = dict(connected=connected, peer_id='AA:BB:CC:DD:EE:FF' if self.legacy else 'phone',
                    display_name=name, name_pending=False)
        self.send('state_snapshot', peer=None if self.legacy and not connected else peer,
                  bluetooth=dict(link_state='connected' if connected else 'disconnected',
                  audio_state='started' if started else 'suspended',
                  player_state='playing' if started else ('unknown' if self.legacy else 'paused')), **extra)

    def offer(self, generation, payload_type=96):
        stream = 'stream-%d' % generation
        self.send('stream_offer', stream_id=stream,
                  peer_id='AA:BB:CC:DD:EE:FF' if self.legacy else 'phone',
                  audio=dict(codec='aac-lc', sample_rate=48000,
                  channels=2, rtp=dict(encoding_name='MPEG4-GENERIC', payload_type=payload_type,
                  ssrc=generation, clock_rate=48000, fmtp=dict(mode='AAC-hbr', config='1190',
                  size_length=13, index_length=3, index_delta_length=3))))
        return stream

    def close(self):
        self.sock.close()


def adts_frames(data):
    frames = []
    offset = 0
    while offset < len(data):
        h = data[offset:offset + 7]
        check(len(h) == 7 and h[0] == 255 and h[1] & 0xf6 == 0xf0, 'invalid ADTS fixture')
        check((h[2] >> 6) == 1 and ((h[2] >> 2) & 15) == 3 and
              (((h[2] & 1) << 2) | (h[3] >> 6)) == 2,
              'fixture must be AAC-LC 48000 Hz stereo (offer config1190)')
        size = ((h[3] & 3) << 11) | (h[4] << 3) | (h[5] >> 5)
        header = 7 if h[1] & 1 else 9
        check(size > header and offset + size <= len(data), 'truncated ADTS fixture')
        frames.append(data[offset + header:offset + size])
        offset += size
    check(frames, 'empty AAC fixture')
    return frames


def main():
    global DEADLINE
    DEADLINE = time.monotonic() + 63
    check(BINARY.is_file(), 'build binary or set RADIOD_BINARY')
    audio = (ROOT / 'tests/fixtures/live.aac').read_bytes()
    frames = adts_frames(audio)
    active = set()
    uri_gets = []
    lock = threading.Lock()

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            token = object()
            with lock:
                active.add(token)
                uri_gets.append(self.path)
            try:
                self.connection.settimeout(.5)
                self.send_response(200)
                self.send_header('Content-Type', 'audio/aac')
                self.end_headers()
                self.wfile.write(audio * 8)
                while not STOP.wait(.022):
                    self.wfile.write(audio)
                    self.wfile.flush()
            except (OSError, TimeoutError):
                pass
            finally:
                with lock:
                    active.discard(token)
        def log_message(self, *_):
            pass

    process = mpd = server = None
    peers = []
    senders = []
    def watchdog(*_):
        raise TimeoutError('70-second receiver watchdog expired')
    previous = signal.signal(signal.SIGALRM, watchdog)
    signal.setitimer(signal.ITIMER_REAL, 70)
    try:
        with tempfile.TemporaryDirectory(prefix='radiod-receiver-') as tmp:
            directory = Path(tmp)
            logpath = directory / 'daemon.log'
            try:
                server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
                server.daemon_threads = True
                threading.Thread(target=lambda: server.serve_forever(poll_interval=.05), daemon=True).start()
                mp, wp, rp = freeport(), freeport(), freeport(True)
                config = dict(listen='127.0.0.1:%d' % mp, initial_volume=8, autostart=False,
                              connect_timeout_seconds=3, stations=[dict(name='Test Radio',
                              url='http://127.0.0.1:%d/live.aac' % server.server_port)],
                              bluetooth=dict(enabled=True, listen='127.0.0.1:%d' % wp,
                              rtp_listen='0.0.0.0:%d' % rp, advertise_address='127.0.0.1',
                              allowed_networks=['127.0.0.0/8'], buffer_ms=500, lease_ms=1500, suspend_ms=300))
                configpath = directory / 'config.yaml'
                configpath.write_text(json.dumps(config))
                statepath = directory / 'state.json'
                with logpath.open('wb') as log:
                    process = subprocess.Popen([str(BINARY), '--config', str(configpath), '--state',
                                                str(statepath), '--null-output'], stdout=log, stderr=log, cwd=directory)
                def connect():
                    nonlocal mpd
                    check(process.poll() is None, 'child exited on startup')
                    try:
                        mpd = MPD(mp)
                        return True
                    except (ConnectionRefusedError, socket.timeout):
                        return False
                eventually(connect, 'MPD startup failed')
                mpd.command('load "radio"')
                radio = mpd.rows()[0]['Id']
                library = mpd.command('listplaylistinfo "radio"')
                ws = WS(wp)
                peers.append(ws)
                check(ws.welcome['volume_percent'] == 8 and ws.welcome['buffer_ms'] == 500,
                      'welcome must retain server volume and source profile')
                check(len(mpd.rows()) == 1, 'hello alone created phone')
                duplicate = WS(wp, rejected=True)
                peers.append(duplicate)
                duplicate.close()
                ws.send('heartbeat')
                ws.wait('heartbeat_ack')
                ws.snapshot(volume_percent=99)
                eventually(lambda: len(mpd.rows()) == 2, 'paused snapshot did not create phone')
                phone = next(r for r in mpd.rows() if r['file'].startswith('bluetooth://'))
                ident = phone['Id']
                check(mpd.rows()[0]['Id'] == ident and phone.get('Pos') == '0',
                      'Bluetooth phone must be first without changing its song ID')
                check([r['Id'] for r in mpd.rows() if not r['file'].startswith('bluetooth://')] == [radio],
                      'Bluetooth priority changed relative radio ordering/IDs')
                check('Actual phone' in phone.get('Name', ''), 'actual phone name missing')
                version = mpd.fields('status')['playlist']
                ws.snapshot()
                ws.hold(.2)
                check(mpd.fields('status')['playlist'] == version, 'snapshot duplicated/invalidated unchanged cache')
                ws.snapshot(name='Renamed phone')
                eventually(lambda: any(r['Id'] == ident and 'Renamed phone' in r.get('Name', '') for r in mpd.rows()),
                           'name update did not preserve ID/update MPD cache')
                check(mpd.command('listplaylistinfo "radio"') == library, 'virtual entry polluted radio playlist')
                available = mpd.rows('listallinfo')
                check(available[0]['file'] == phone['file'], 'Bluetooth must lead the available MPD catalogue too')
                check(len(available) == 2 and sum(r['file'].startswith('bluetooth://') for r in available) == 1,
                      'MPD library lost radio or duplicated virtual phone')
                check(mpd.fields('status').get('radiod_state') == 'stop', 'paused presence started audio')
                check(mpd.fields('status')['volume'] == '8', 'cached phone volume overwrote server')
                mpd.command('play')  # Resume normal radio, not an explicit radio-selection override.
                eventually(lambda: (ws.pump() or True) and mpd.fields('status').get('radiod_state') == 'play',
                           'synthetic radio did not decode')
                ws.snapshot(started=True)
                stream = ws.offer(1)
                accept = ws.wait('stream_accept')
                check(accept['stream_id'] == stream and accept['buffer_ms'] == 500 and
                      accept['destination'] == dict(host='127.0.0.1', port=rp), 'incorrect accepted RTP profile')
                ws.hold(.3)
                check(mpd.fields('status').get('radiod_state') != 'play', 'empty live RTP pipeline falsely playing')

                def rtp(seconds, ip='127.0.0.1', ssrc=1, pt=96):
                    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
                        udp.bind((ip, 0))
                        end = time.monotonic() + seconds
                        seq = 0
                        while time.monotonic() < end and not STOP.is_set():
                            au = frames[seq % len(frames)]
                            payload = b'\x00\x10' + struct.pack('!H', len(au) << 3) + au
                            udp.sendto(struct.pack('!BBHII', 0x80, 0x80 | pt, seq & 65535, (seq * 1024) & 0xffffffff, ssrc) + payload,
                                       ('127.0.0.1', rp))
                            seq += 1
                            STOP.wait(1024 / 48000)
                for kwargs in (dict(ip='127.0.0.2'), dict(ssrc=1234), dict(pt=97)):
                    rtp(.22, **kwargs)
                    ws.hold(.05)
                    check(mpd.fields('status').get('radiod_state') != 'play', 'wrong IP/SSRC/PT RTP decoded')
                def launch(generation):
                    thread = threading.Thread(target=lambda: rtp(3, ssrc=generation), daemon=True)
                    thread.start()
                    senders.append(thread)
                launch(1)
                def actual_play():
                    ws.pump()
                    return mpd.fields('status').get('radiod_state') == 'play'
                eventually(actual_play, 'valid AAC RTP never decoded (AAC runtime codecs mandatory)')
                ws.wait('player_state', lambda m: m['source'] == 'bluetooth')
                ws.snapshot(started=True, stream_id=stream, metadata=dict(title='Receiver Title', artist='Receiver Artist'))
                eventually(lambda: mpd.fields('currentsong').get('Title') == 'Receiver Title' and
                           mpd.fields('currentsong').get('Artist') == 'Receiver Artist', 'correlated metadata missing')
                ws.snapshot(started=True, stream_id='stale-generation',
                            metadata=dict(title='Wrong generation', artist='Wrong artist'))
                ws.hold(.15)
                check(mpd.fields('currentsong').get('Title') == 'Receiver Title',
                      'stale-generation metadata overwrote active stream')
                ws.snapshot()
                eventually(lambda: (ws.pump() or True) and mpd.fields('currentsong').get('Id') == radio,
                           'automatic phone pause did not return prior radio')
                mpd.command('playid ' + ident)
                ws.wait('command', lambda m: m['action'] == 'play')
                ws.snapshot(started=True)
                stream = ws.offer(2)
                ws.wait('stream_accept', lambda m: m['stream_id'] == stream)
                launch(2)
                eventually(actual_play, 'fresh explicit generation did not decode')
                # Synchronize once, then send the slider burst without waiting for ACKs.
                mpd.command('setvol 17')
                baseline = ws.wait('player_state', lambda m: m['volume_percent'] == 17)
                ws.send('volume_synced', revision=baseline['revision'])
                session = ws.session
                with lock:
                    gets_before = len(uri_gets)
                for index, value in enumerate((38, 76, 114)):
                    ws.send('volume_request', id='slider-%d' % index, stream_id=stream,
                            origin='phone', absolute_127=value)
                for index, expected in enumerate((30, 60, 90)):
                    latest = ws.wait('player_state', lambda m, i=index:
                                     m.get('ack_id') == 'slider-%d' % i)
                    check(latest['volume_percent'] == expected and latest.get('accepted') is not False,
                          'rapid slider request rejected or applied out of order')
                check(latest['revision'] > baseline['revision'], 'slider revision did not advance')
                ws.send('volume_synced', revision=baseline['revision'])
                stale = ws.wait('player_state', lambda m: m.get('accepted') is False)
                check(stale['volume_percent'] == 90 and stale['active_stream_id'] == stream,
                      'known stale volume ACK changed latest target/stream')
                ws.send('volume_synced', revision=latest['revision'] + 1000000)
                unknown = ws.wait('player_state', lambda m: m.get('accepted') is False)
                check(unknown['volume_percent'] == 90 and unknown['active_stream_id'] == stream and
                      unknown['desired_transport'] == stale['desired_transport'] == 'play',
                      'unknown volume ACK changed target/stream or paused transport')
                ws.send('heartbeat')
                ws.wait('heartbeat_ack')
                check(ws.session == session and mpd.fields('status')['volume'] == '90' and
                      mpd.fields('status').get('radiod_state') == 'play' and
                      mpd.fields('currentsong')['Id'] == ident,
                      'late/unknown ACK replaced session, paused/stopped audio, or changed owner')
                check(stale['selected_receiver_id'] == unknown['selected_receiver_id'] == ws.device and
                      stale['selected_peer_id'] == unknown['selected_peer_id'] == 'phone',
                      'playing selected receiver/peer identity missing')
                with lock:
                    check(len(uri_gets) == gets_before, 'volume ACK reopened URI source')
                check(not any(m.get('type') == 'error' or
                              (m.get('seq', 0) > baseline['seq'] and
                               m.get('type') in ('welcome', 'command', 'stream_accept'))
                              for m in ws.messages), 'volume ACK restarted receiver transport/session')
                mpd.command('pause 1')
                check(mpd.fields('status').get('radiod_state') == 'pause', 'local pause waited for remote ACK')
                ws.hold(.5)
                check(mpd.fields('currentsong')['Id'] == ident, 'manual pause unexpectedly auto-returned radio')
                paused = ws.wait('player_state', lambda m: m['desired_transport'] == 'pause' and
                                 m['revision'] > latest['revision'] and m.get('active_stream_id') is None)
                check(paused['selected_receiver_id'] == ws.device and paused['selected_peer_id'] == 'phone',
                      'pause cleared selected receiver/peer identity')
                ws.send('volume_request', id='paused-owner', peer_id='phone',
                        origin='phone', absolute_127=51)
                paused_ack = ws.wait('player_state', lambda m: m.get('ack_id') == 'paused-owner')
                check(paused_ack['volume_percent'] == 40 and paused_ack.get('accepted') is not False and
                      paused_ack['desired_transport'] == 'pause' and paused_ack['active_stream_id'] is None and
                      paused_ack['selected_receiver_id'] == ws.device and paused_ack['selected_peer_id'] == 'phone',
                      'paused peer-scoped owner volume rejected, resumed, or lost selection')
                paused_rival = WS(wp, device='paused-other-receiver')
                peers.append(paused_rival)
                paused_rival.snapshot(name='Paused other phone')
                rival_baseline = paused_rival.wait('player_state', lambda m: m['volume_percent'] == 40)
                paused_rival.send('volume_synced', revision=rival_baseline['revision'])
                paused_rival.send('volume_request', id='paused-nonowner', peer_id='phone',
                                  origin='phone', absolute_127=127)
                denied = paused_rival.wait('player_state', lambda m: m.get('ack_id') == 'paused-nonowner')
                check(denied.get('accepted') is False and denied['volume_percent'] == 40 and
                      denied['selected_receiver_id'] == ws.device and denied['selected_peer_id'] == 'phone',
                      'paused nonselected peer changed volume or selected identity')
                paused_rival.send('heartbeat')
                paused_rival.wait('heartbeat_ack')
                paused_rival.snapshot(connected=False)
                paused_rival.send('heartbeat')
                paused_rival.wait('heartbeat_ack')
                paused_rival.close()
                ws.send('heartbeat')
                ws.wait('heartbeat_ack')
                check(mpd.fields('status')['volume'] == '40' and
                      mpd.fields('status').get('radiod_state') == 'pause' and
                      mpd.fields('currentsong')['Id'] == ident,
                      'paused volume request resumed audio or changed selected song')
                with lock:
                    check(len(uri_gets) == gets_before, 'paused volume request reopened URI source')
                # Preserve the established baseline for the existing lifecycle cases.
                mpd.command('setvol 8')
                eventually(lambda: (ws.pump() or True) and not active,
                           'previous radio HTTP GET remained active after takeover', timeout=3)
                mpd.command('stop')
                old = ws.offer(2)
                ws.wait('stream_reject', lambda m: m['stream_id'] == old)
                # A new generation must not bypass the local stop fence either.
                ws.offer(8)
                ws.wait('stream_reject', lambda m: m['stream_id'] == 'stream-8')
                check(mpd.fields('status').get('radiod_state') == 'stop', 'late offer undid local stop')
                mpd.command('playid ' + ident)
                ws.wait('command', lambda m: m['action'] == 'play')
                stream = ws.offer(3)
                ws.wait('stream_accept', lambda m: m['stream_id'] == stream)
                rival = WS(wp, device='other-receiver')
                peers.append(rival)
                rival.snapshot(name='Other phone', started=True)
                rival.offer(99)
                rival.wait('stream_reject', lambda m: m['stream_id'] == 'stream-99')
                check(mpd.fields('currentsong').get('Id') == ident,
                      'second connected receiver stole active Bluetooth owner')
                rival.snapshot(connected=False)
                rival.send('heartbeat')
                rival.wait('heartbeat_ack')
                rival.close()
                mpd.command('setvol 23')
                state = ws.wait('player_state', lambda m: m['volume_percent'] == 23)
                ws.send('volume_synced', revision=state['revision'])
                ws.send('volume_request', id='volume-one', stream_id=stream, origin='phone', absolute_127=64)
                ws.wait('player_state', lambda m: m.get('ack_id') == 'volume-one' and m['volume_percent'] == 50)
                check(mpd.fields('status')['volume'] == '50', 'phone volume diverged from shared Core mixer')
                ws.snapshot(connected=False)
                eventually(lambda: len(mpd.rows()) == 1, 'peer disconnect did not remove virtual entry')
                check(mpd.fields('status').get('radiod_state') != 'play', 'disconnect restored invalidated radio play wish')
                ws.snapshot()
                eventually(lambda: len(mpd.rows()) == 2, 'phone reconnect presence missing')
                ident = next(r['Id'] for r in mpd.rows() if r['file'].startswith('bluetooth://'))
                ws.close()
                time.sleep(.15)
                ws = WS(wp)
                peers.append(ws)
                ws.snapshot()
                ws.hold(.2)
                check(next(r['Id'] for r in mpd.rows() if r['file'].startswith('bluetooth://')) == ident,
                      'same boot reconnect changed stable ID')
                ws.close()
                eventually(lambda: len(mpd.rows()) == 1, 'lease expiration failed to remove phone', timeout=3)
                for field, bad in [('control_session_id', 'unknown'), ('boot_id', 'old'), ('seq', 1)]:
                    invalid = WS(wp, device='invalid-' + field)
                    peers.append(invalid)
                    envelope = dict(v=1, type='state_snapshot', seq=2, boot_id='boot',
                                    control_session_id=invalid.session, peer=dict(connected=True, peer_id='bad',
                                    display_name='Must not appear', name_pending=False), bluetooth=dict(
                                    link_state='connected', audio_state='suspended', player_state='paused'))
                    envelope[field] = bad
                    invalid.raw(envelope)
                    invalid.wait('error')
                    check(len(mpd.rows()) == 1, 'invalid envelope mutated phone directory')
                    invalid.close()
                incapable = WS(wp, device='no-absolute', absolute=False)
                peers.append(incapable)
                incapable.snapshot()
                eventually(lambda: len(mpd.rows()) == 2, 'incapable phone presence missing')
                incapable_id = next(r['Id'] for r in mpd.rows() if r['file'].startswith('bluetooth://'))
                mpd.command('playid ' + incapable_id)
                incapable.wait('command', lambda m: m['action'] == 'play')
                incapable.offer(41)
                incapable.wait('stream_accept')
                # Drain to the current authoritative revision after selection/offer.
                mpd.command('setvol 31')
                latest = incapable.wait('player_state', lambda m: m['volume_percent'] == 31)
                incapable.send('volume_synced', revision=latest['revision'])
                incapable.send('volume_request', id='unsupported', stream_id='stream-41',
                               origin='phone', absolute_127=127)
                incapable.wait('player_state', lambda m: m.get('ack_id') == 'unsupported' and
                               m.get('accepted') is False and m['volume_percent'] == 31)
                incapable.send('heartbeat')
                incapable.wait('heartbeat_ack')
                check(mpd.fields('status')['volume'] == '31', 'unsupported absolute volume changed shared mixer')
                incapable.snapshot(connected=False)
                incapable.send('heartbeat')
                incapable.wait('heartbeat_ack')
                incapable.close()
                eventually(lambda: len(mpd.rows()) == 1, 'incapable peer disconnect failed')

                # Actual ESP firmware wire dialect, independently of the strict API above.
                legacy = WS(wp, device='actual-firmware-dialect', legacy=True)
                peers.append(legacy)
                check(legacy.welcome['heartbeat_interval_ms'] == legacy.welcome['heartbeat_ms'] == 500 and
                      legacy.welcome['lease_timeout_ms'] == legacy.welcome['lease_ms'] == 1500,
                      'firmware welcome aliases missing or inconsistent')
                baseline = legacy.wait('player_state')
                check(baseline['volume_percent'] == 31, 'firmware server volume baseline missing')
                check(len(mpd.rows()) == 1, 'firmware hello alone created phone')
                legacy.snapshot(name='Actual ESP phone', capabilities=dict(
                                transport_control=True, absolute_volume=True),
                                metadata=dict(title=None, artist=None))
                eventually(lambda: len(mpd.rows()) == 2, 'firmware MAC/unknown-state snapshot missing')
                legacy_phone = next(r for r in mpd.rows() if r['file'].startswith('bluetooth://'))
                check('Actual ESP phone' in legacy_phone.get('Name', ''), 'firmware actual name absent')
                check(mpd.fields('status').get('radiod_state') != 'play', 'unknown AVRCP state inferred playback')
                legacy.send('volume_request', id='legacy-inactive', stream_id='not-active',
                            origin='phone', absolute_127=127)
                legacy.wait('player_state', lambda m: m.get('ack_id') == 'legacy-inactive' and
                            m.get('accepted') is False and m['volume_percent'] == 31)
                mpd.command('playid ' + legacy_phone['Id'])
                command = legacy.wait('command', lambda m: m['action'] == 'play')
                legacy.send('command_result', id=command['id'], status='accepted')
                legacy.snapshot(name='Actual ESP phone', started=True)
                legacy_stream = legacy.offer(61, payload_type=97)
                legacy.wait('stream_accept', lambda m: m['stream_id'] == legacy_stream)
                legacy_sender = threading.Thread(target=lambda: rtp(3, ssrc=61, pt=97), daemon=True)
                legacy_sender.start()
                senders.append(legacy_sender)
                eventually(lambda: (legacy.pump() or True) and
                           mpd.fields('status').get('radiod_state') == 'play',
                           'actual firmware AAC PT97 stream never decoded')
                legacy.wait('player_state', lambda m: m.get('stream_id') == legacy_stream and
                            m.get('active_stream_id') == legacy_stream and m['source'] == 'bluetooth')
                # Legacy firmware has no volume_synced message: its delivered baseline
                # plus accepted active generation permits actual phone requests, not cache.
                legacy.send('volume_request', id='legacy-active', stream_id=legacy_stream,
                            origin='phone', absolute_127=25)
                legacy.wait('player_state', lambda m: m.get('ack_id') == 'legacy-active' and
                            m['volume_percent'] == 20)
                check(mpd.fields('status')['volume'] == '20', 'firmware active volume not shared with MPD')
                legacy.snapshot(name='Actual ESP phone', started=True, capabilities=dict(
                                transport_control=True, absolute_volume=False))
                legacy.send('volume_request', id='legacy-capability-denied', stream_id=legacy_stream,
                            origin='phone', absolute_127=127)
                legacy.wait('player_state', lambda m: m.get('ack_id') == 'legacy-capability-denied' and
                            m.get('accepted') is False and m['volume_percent'] == 20)
                legacy.send('heartbeat')
                legacy.wait('heartbeat_ack')
                check(mpd.fields('status')['volume'] == '20', 'firmware capability-false request raised volume')
                legacy.snapshot(connected=False)
                legacy.send('heartbeat')
                legacy.wait('heartbeat_ack')
                eventually(lambda: len(mpd.rows()) == 1, 'firmware peer:null did not remove phone')
                legacy.close()
                check(mpd.command('listplaylistinfo "radio"') == library, 'radio playlist changed')
                check(statepath.is_file(), 'daemon did not persist requested local state')
                check('bluetooth://' not in statepath.read_text(), 'ephemeral Bluetooth URL persisted')
                print('receiver: PASS (actual binary, null output, AAC RTP, strict/firmware wire, policy, volume, presence, lease)')
            except BaseException:
                if logpath.exists():
                    print('--- child radiod log (synthetic local sources only) ---', flush=True)
                    print(logpath.read_text(errors='replace'), flush=True)
                raise
            finally:
                STOP.set()
                for peer in peers:
                    peer.close()
                if mpd:
                    mpd.close()
                if process:
                    process.terminate()
                    try:
                        process.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)
                if server:
                    server.shutdown()
                    server.server_close()
                for thread in senders:
                    thread.join(timeout=.2)
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)


if __name__ == '__main__':
    main()
