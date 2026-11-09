#!/usr/bin/env python3
"""Offline real-binary UPnP/MPD integration regression (stdlib only).

RADIOD_BINARY overrides target/debug/radiod. The renderer and MPD each use
an independently allocated local port. No traffic outside 127/8 is required.
"""
import http.client
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
import uuid
import xml.etree.ElementTree as ET
from xml.sax.saxutils import escape

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('RADIOD_BINARY', str(ROOT / 'target/debug/radiod'))).resolve()
STOP = threading.Event()
DEADLINE = 0
HTTP_PORT = 8091
AV = 'AVTransport'
RC = 'RenderingControl'


def check(value, message):
    if not value:
        raise AssertionError(message)


def remaining(cap=2):
    value = min(cap, DEADLINE - time.monotonic())
    if value <= 0:
        raise TimeoutError('UPnP overall deadline expired')
    return value


def eventually(predicate, message, timeout=5):
    end = min(DEADLINE, time.monotonic() + timeout)
    while True:
        if predicate():
            return
        check(time.monotonic() < end, message)
        STOP.wait(min(.04, max(0, end - time.monotonic())))


def local(tag):
    return tag.rsplit('}', 1)[-1]


def fields(xml):
    return {local(e.tag): e.text or '' for e in xml.iter()}


def request(method, path, body=None, headers=None, owner='127.0.0.1'):
    # Binding the controller socket is essential to exercise native peer policy.
    conn = http.client.HTTPConnection('127.0.0.1', HTTP_PORT, timeout=remaining(),
                                      source_address=(owner, 0))
    try:
        conn.request(method, path, body, headers or {})
        response = conn.getresponse()
        return response.status, dict(response.getheaders()), response.read(1024 * 1024)
    finally:
        conn.close()


def soap(service, action, args=None, fault=None, owner='127.0.0.1'):
    args = {} if args is None else args
    urn = 'urn:schemas-upnp-org:service:%s:1' % service
    body = ('<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" '
            's:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>'
            '<u:%s xmlns:u="%s">%s</u:%s></s:Body></s:Envelope>' %
            (action, urn, ''.join('<%s>%s</%s>' % (k, escape(str(v)), k)
                                 for k, v in args.items()), action)).encode()
    status, _, data = request('POST', '/%s/control' % service, body,
                             {'Content-Type': 'text/xml; charset="utf-8"',
                              'SOAPACTION': '"%s#%s"' % (urn, action)}, owner)
    try:
        doc = ET.fromstring(data)
    except ET.ParseError as error:
        raise AssertionError('%s HTTP %s returned invalid SOAP: %r' % (action, status, data[:500])) from error
    result = fields(doc)
    if fault is not None:
        check(status == 500 and 'Fault' in result and 'errorCode' in result,
              '%s expected SOAP fault, got HTTP %s: %r' % (action, status, result))
        check(fault is True or result['errorCode'] == str(fault),
              '%s expected fault %s, got %r' % (action, fault, result))
    else:
        check(status == 200 and 'Fault' not in result and action + 'Response' in result,
              '%s failed HTTP %s: %r' % (action, status, result))
    return result


def av(action, **args):
    return soap(AV, action, dict(InstanceID=0, **args))


def rc(action, **args):
    return soap(RC, action, dict(InstanceID=0, Channel='Master', **args))


class MPD:
    def __init__(self, port):
        self.sock = socket.create_connection(('127.0.0.1', port), remaining())
        self.file = self.sock.makefile('rb')
        check(self.line().startswith('OK MPD '), 'missing MPD greeting')

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
            check(not line.startswith('ACK '), command + ': ' + line)
            lines.append(line)

    def fields(self, command):
        return dict(line.split(': ', 1) for line in self.command(command) if ': ' in line)

    def close(self):
        self.file.close()
        self.sock.close()


def records(lines):
    result = []
    for line in lines:
        key, value = line.split(': ', 1)
        if key == 'file':
            result.append({})
        result[-1][key] = value
    return result


def main():
    global DEADLINE, HTTP_PORT
    DEADLINE = time.monotonic() + 64
    check(BINARY.is_file(), 'run cargo build or set RADIOD_BINARY')
    notifications = []
    condition = threading.Condition()
    fixture_errors = []
    requests = {}
    eof = threading.Event()
    # Reuse the checked-in synthetic ADTS fixture: complete-frame repetition
    # supports both indefinite radio and true EOF without WAV length seeking.
    audio = (ROOT / 'tests/fixtures/live.aac').read_bytes()
    check(0 < len(audio) <= 40960, 'missing/oversized synthetic AAC fixture')

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_NOTIFY(self):
            try:
                self.connection.settimeout(remaining())
                check(self.path in ('/notify/av', '/notify/rc'), 'unexpected GENA callback path')
                size = int(self.headers.get('Content-Length', '0'))
                check(0 < size <= 65536, 'invalid GENA body length')
                doc = ET.fromstring(self.rfile.read(size))
                values = fields(doc)
                change = ET.fromstring(values['LastChange'])
                check(local(change.tag) == 'Event', 'LastChange is not an Event')
                check(self.headers.get('NT') == 'upnp:event' and
                      self.headers.get('NTS') == 'upnp:propchange', 'invalid GENA headers')
                with condition:
                    notifications.append((self.path, {k.lower(): v for k, v in self.headers.items()}, change))
                    condition.notify_all()
                self.send_response(200)
                self.end_headers()
            except Exception as error:
                with condition:
                    fixture_errors.append(repr(error))
                    condition.notify_all()
                self.send_error(400)

        def do_GET(self):
            self.connection.settimeout(1)
            with condition:
                requests[self.path] = requests.get(self.path, 0) + 1
                condition.notify_all()
            if self.path == '/error.aac':
                self.send_error(503, 'Synthetic failure')
                return
            if self.path not in ('/radio1.aac', '/radio2.aac', '/foreign.wav', '/eof.aac'):
                self.send_error(404)
                return
            try:
                if self.path == '/foreign.wav':
                    # AriaCast's live PCM wire format: unknown RIFF/data length,
                    # 48 kHz stereo S16LE, no Content-Length, ignore Range.
                    wav = (b'RIFF' + struct.pack('<I', 0xffffffff) + b'WAVEfmt ' +
                           struct.pack('<IHHIIHH', 16, 1, 2, 48000, 192000, 4, 16) +
                           b'data' + struct.pack('<I', 0xffffffff))
                    self.send_response(200)
                    self.send_header('Content-Type', 'audio/wav')
                    self.end_headers()
                    self.wfile.write(wav + bytes(192000))  # One-second initial PCM burst.
                    while not STOP.is_set():
                        self.wfile.write(bytes(19200))
                        self.wfile.flush()
                        if STOP.wait(.1):
                            return
                    return
                self.send_response(200)
                self.send_header('Content-Type', 'audio/aac')
                self.end_headers()
                self.wfile.write(audio)  # Two seconds of bounded preroll.
                self.wfile.flush()
                position = 0
                amount = max(1, len(audio) // 20)
                while not STOP.wait(.1):
                    if self.path == '/eof.aac' and eof.is_set():
                        # Finish the current complete ADTS fixture before EOF.
                        self.wfile.write(audio[position:])
                        self.wfile.flush()
                        return
                    chunk = (audio + audio)[position:position + amount]
                    self.wfile.write(chunk)
                    position = (position + amount) % len(audio)
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, socket.timeout):
                pass
            except Exception as error:
                fixture_errors.append(repr(error))

    server = process = client = None
    subscriptions = []
    old_handler = signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError('70s watchdog expired')))
    signal.setitimer(signal.ITIMER_REAL, 70)
    try:
        with tempfile.TemporaryDirectory(prefix='radiod-upnp-test-') as directory:
            directory = Path(directory)
            log_path = directory / 'daemon.log'
            try:
                server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
                server.daemon_threads = True
                threading.Thread(target=lambda: server.serve_forever(poll_interval=.05), daemon=True).start()
                base = 'http://127.0.0.1:%d' % server.server_port
                with socket.socket() as reservation, socket.socket() as http_reservation:
                    reservation.bind(('127.0.0.1', 0))
                    http_reservation.bind(('127.0.0.1', 0))
                    port = reservation.getsockname()[1]
                    HTTP_PORT = http_reservation.getsockname()[1]
                config = directory / 'radio.yaml'
                state = directory / 'state.json'
                config.write_text('listen: "127.0.0.1:%d"\ninitial_volume: 20\n'
                                  'autostart: false\nconnect_timeout_seconds: 5\nretry_max_seconds: 1\n'
                                  f'upnp:\n  enabled: true\n  interface: lo\n  bind_address: 127.0.0.1\n  port: {HTTP_PORT}\n'
                                  '  friendly_name: "Offline UPnP & renderer"\n'
                                  '  allowed_networks: [127.0.0.0/8]\n'
                                  '  prepared_timeout_seconds: 2\n  failure_timeout_seconds: 2\n'
                                  'stations:\n' % port + ''.join(
                                      '  - name: "Synthetic radio %d"\n    url: "%s/radio%d.aac"\n' %
                                      (i, base, i) for i in (1, 2)))
                with log_path.open('wb') as log:
                    process = subprocess.Popen([str(BINARY), '--config', str(config), '--state', str(state),
                                                '--null-output'], stdout=log, stderr=log, cwd=directory)

                def connect():
                    nonlocal client
                    check(process.poll() is None, 'radiod exited during startup (native GUPnP?)')
                    try:
                        client = MPD(port)
                        return True
                    except (ConnectionRefusedError, TimeoutError):
                        return False

                eventually(connect, 'MPD startup failed', 12)
                status, _, data = request('GET', '/device.xml')
                check(status == 200, 'device.xml unavailable')
                description = ET.fromstring(data)
                values = fields(description)
                check(values['friendlyName'] == 'Offline UPnP & renderer', 'friendlyName not rendered')
                check(b'@NAME@' not in data and b'@UDN@' not in data, 'unexpanded device placeholders')
                check(values['UDN'].startswith('uuid:'), 'UDN lacks uuid prefix')
                uuid.UUID(values['UDN'][5:])
                services = [fields(e) for e in description.iter() if local(e.tag) == 'service']
                check(len(services) == 3, 'expected three standard services')
                expected = {AV: {'SetAVTransportURI', 'Play', 'Pause', 'Stop', 'GetTransportInfo',
                                 'GetMediaInfo', 'GetPositionInfo'},
                            RC: {'SetVolume', 'GetVolume', 'SetMute', 'GetMute'},
                            'ConnectionManager': {'GetProtocolInfo', 'GetCurrentConnectionIDs', 'GetCurrentConnectionInfo'}}
                for service in services:
                    name = service['serviceType'].split(':')[-2]
                    check(name in expected and service['controlURL'] == '/' + name + '/control' and
                          service['eventSubURL'] == '/' + name + '/event', 'incorrect service URLs')
                    status, _, scpd = request('GET', service['SCPDURL'])
                    check(status == 200, name + ' SCPD missing')
                    doc = ET.fromstring(scpd)
                    check(local(doc.tag) == 'scpd', 'invalid SCPD root')
                    actions = {next(c.text for c in e if local(c.tag) == 'name')
                               for e in doc.iter() if local(e.tag) == 'action'}
                    check(expected[name] <= actions, name + ' missing standard actions')
                    variables = {fields(e)['name'] for e in doc.iter() if local(e.tag) == 'stateVariable'}
                    check(all(e.text in variables for e in doc.iter() if local(e.tag) == 'relatedStateVariable'),
                          name + ' unresolved state variable')
                check('http-get:' in soap('ConnectionManager', 'GetProtocolInfo')['Sink'], 'missing sink protocols')

                def notify(path, variable, value=None, start=0, timeout=3):
                    end = min(DEADLINE, time.monotonic() + timeout)
                    with condition:
                        while True:
                            check(not fixture_errors, 'callback fixture failed: %r' % fixture_errors)
                            for p, headers, change in notifications[start:]:
                                if p == path and any(local(e.tag) == variable and
                                                     (value is None or e.get('val') == value)
                                                     for e in change.iter()):
                                    return headers
                            left = end - time.monotonic()
                            check(left > 0, 'GENA missing %s=%s on %s' % (variable, value, path))
                            condition.wait(left)

                for service, suffix, variable in ((AV, 'av', 'TransportState'), (RC, 'rc', 'Volume')):
                    status, headers, _ = request('SUBSCRIBE', '/' + service + '/event', headers={
                        'CALLBACK': '<%s/notify/%s>' % (base, suffix), 'NT': 'upnp:event', 'TIMEOUT': 'Second-120'})
                    headers = {k.lower(): v for k, v in headers.items()}
                    check(status == 200 and headers.get('sid', '').startswith('uuid:'), 'GENA SUBSCRIBE failed')
                    subscriptions.append((service, headers['sid']))
                    initial = notify('/notify/' + suffix, variable)
                    check(initial.get('sid') == headers['sid'] and initial.get('seq') == '0', 'invalid initial GENA SID/SEQ')

                client.command('load "radio"')
                original = records(client.command('playlistinfo'))
                check(len(original) == 2, 'expected exactly two synthetic stations')
                ids = [e['Id'] for e in original]

                def state_is(source, actual, song=None):
                    s = client.fields('status')
                    return (s.get('radiod_source') == source and s.get('radiod_state') == actual and
                            (song is None or s.get('songid') == song))

                def wait_radio(song, actual='play'):
                    eventually(lambda: state_is('radio', actual, song), 'radio failed to return: ' + repr(client.fields('status')))

                title, artist = 'Foreign <title> & test', 'Synthetic & artist'
                metadata = ('<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" '
                            'xmlns:dc="http://purl.org/dc/elements/1.1/" '
                            'xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
                            '<item id="0" parentID="-1" restricted="1"><dc:title>%s</dc:title>'
                            '<upnp:artist>%s</upnp:artist>'
                            '<res protocolInfo="http-get:*:audio/x-wav:*">%s</res>'
                            '</item></DIDL-Lite>') % (escape(title), escape(artist), escape(base + '/foreign.wav'))

                def prepare(path='/foreign.wav'):
                    media_metadata = metadata
                    if path.endswith('.aac'):
                        media_metadata = metadata.replace('audio/x-wav', 'audio/aac').replace(
                            escape(base + '/foreign.wav'), escape(base + path))
                    av('SetAVTransportURI', CurrentURI=base + path, CurrentURIMetaData=media_metadata)
                    s = client.fields('status')
                    check(s.get('radiod_source') == 'upnp' and int(s.get('radiod_session', '0')) > 0,
                          'MPD missing UPnP source/session')
                    return s

                def play_foreign(path='/foreign.wav'):
                    s = prepare(path)
                    av('Play', Speed=1)
                    eventually(lambda: state_is('upnp', 'play', s['songid']), 'foreign audio never actually played')
                    return s

                # Negative argument validation must produce structured UPnP faults.
                soap(AV, 'GetTransportInfo', {'InstanceID': 1}, fault=718)
                soap(AV, 'Play', {'InstanceID': 0, 'Speed': 2}, fault=717)
                soap(AV, 'SetAVTransportURI', {'InstanceID': 0, 'CurrentURI': 'file:///not-allowed',
                                             'CurrentURIMetaData': ''}, fault=True)
                soap(RC, 'SetVolume', {'InstanceID': 0, 'Channel': 'Master'}, fault=402)

                # A stopped radio stays stopped after a foreign session.
                s = play_foreign()
                check(av('GetTransportInfo')['CurrentTransportState'] == 'PLAYING', 'transport query not PLAYING')
                check(av('GetMediaInfo')['CurrentURI'] == base + '/foreign.wav', 'media URI mismatch')
                position = av('GetPositionInfo')
                check(position['TrackURI'] == base + '/foreign.wav', 'position URI mismatch')
                check(fields(ET.fromstring(position['TrackMetaData']))['title'] == title, 'DIDL title query mismatch')
                current = client.fields('currentsong')
                check(current.get('Name') == 'UPnP' and current.get('Title') == title and current.get('Artist') == artist,
                      'ghost metadata incorrect')
                with condition:
                    foreign_gets = requests.get('/foreign.wav', 0)
                check(foreign_gets == 1, 'initial WAV playback issued unexpected HTTP GETs')
                # Receiver volume is control-plane/hardware state, not PCM queued
                # before the stream buffer. It must not reopen or pause audio.
                retained_volume = soap(RC, 'GetVolume', {'InstanceID': 0, 'Channel': 'Master'})['CurrentVolume']
                changed_at = time.monotonic()
                soap(RC, 'SetVolume', {'InstanceID': 0, 'Channel': 'Master', 'DesiredVolume': retained_volume})
                volume_roundtrip = time.monotonic() - changed_at
                check(volume_roundtrip < .5, 'receiver volume waited behind audio buffer: %.3fs' % volume_roundtrip)
                check(state_is('upnp', 'play', s['songid']) and
                      client.fields('status')['radiod_session'] == s['radiod_session'],
                      'direct volume control restarted or paused source')
                with condition:
                    check(requests.get('/foreign.wav', 0) == foreign_gets, 'direct volume control reopened source')
                updated_title = 'Live metadata update'
                updated = metadata.replace(escape(title), updated_title)
                av('SetAVTransportURI', CurrentURI=base + '/foreign.wav', CurrentURIMetaData=updated)
                av('Play', Speed=1)
                check(state_is('upnp', 'play', s['songid']), 'redundant Play restarted live WAV')
                check(client.fields('status')['radiod_session'] == s['radiod_session'] and
                      client.fields('currentsong').get('Title') == updated_title,
                      'same-URI metadata update changed session or lost title')
                check(fields(ET.fromstring(av('GetPositionInfo')['TrackMetaData']))['title'] == updated_title,
                      'same-URI DIDL update not reflected in query')
                with condition:
                    check(requests.get('/foreign.wav', 0) == foreign_gets,
                          'same-URI metadata update/redundant Play reopened HTTP source')
                queue = records(client.command('playlistinfo'))
                check(len(queue) == 3 and [e['Id'] for e in queue[:2]] == ids and queue[-1]['Id'] == s['songid'],
                      'ghost queue visibility/stable IDs incorrect')
                check(len(records(client.command('listplaylistinfo radio'))) == 2 and
                      client.fields('stats').get('songs') == '2' and
                      len(records(client.command('listallinfo'))) == 2, 'ghost leaked into radio library')
                saved = state.read_text()
                check(base + '/foreign.wav' not in saved and 'session' not in saved.lower() and 'UPnP' not in saved,
                      'foreign Session/ghost saved in JSON')
                json.loads(saved)
                oldsession = s['radiod_session']
                soap(AV, 'SetAVTransportURI', {'InstanceID': 0, 'CurrentURI': base + '/radio2.aac',
                                             'CurrentURIMetaData': metadata}, fault=True, owner='127.0.0.2')
                check(client.fields('status').get('radiod_session') == oldsession and
                      client.fields('currentsong').get('file') == base + '/foreign.wav', 'second owner corrupted session')
                with condition:
                    check(requests.get('/foreign.wav', 0) == foreign_gets,
                          'metadata update or rejected owner triggered delayed source reconnect')
                marker = len(notifications)
                av('Pause')
                eventually(lambda: state_is('upnp', 'pause'), 'Pause not reflected in MPD')
                check(av('GetTransportInfo')['CurrentTransportState'] == 'PAUSED_PLAYBACK', 'Pause query incorrect')
                notify('/notify/av', 'TransportState', 'PAUSED_PLAYBACK', marker)
                av('Play', Speed=1)
                eventually(lambda: state_is('upnp', 'play'), 'resume did not actually play')
                av('Stop')
                eventually(lambda: state_is('radio', 'stop'), 'stopped radio resumed unexpectedly')

                # Shared full-range volume/mute, including actual GENA changes.
                marker = len(notifications)
                rc('SetVolume', DesiredVolume=100)
                check(rc('GetVolume')['CurrentVolume'] == '100' and client.fields('status')['volume'] == '100', 'full-range volume not shared')
                notify('/notify/rc', 'Volume', '100', marker)
                rc('SetMute', DesiredMute=1)
                check(rc('GetMute')['CurrentMute'] in ('1', 'true') and rc('GetVolume')['CurrentVolume'] == '100' and
                      client.fields('status')['volume'] == '0', 'mute lost retained volume')
                client.command('setvol 31')
                check(rc('GetVolume')['CurrentVolume'] == '31' and rc('GetMute')['CurrentMute'] in ('0', 'false'),
                      'MPD setvol failed to unmute')

                client.command('playid ' + ids[0])
                wait_radio(ids[0])
                play_foreign()
                av('Stop')
                wait_radio(ids[0])
                # Fixture-controlled EOF and synthetic HTTP 503 both release takeover.
                play_foreign('/eof.aac')
                eof.set()
                wait_radio(ids[0])
                prepare('/error.aac')
                av('Play', Speed=1)
                wait_radio(ids[0])
                # Prepared lease expires without requiring any Play request.
                prepare()
                wait_radio(ids[0])
                play_foreign()
                client.command('stop')
                eventually(lambda: state_is('radio', 'stop'), 'global MPD stop retained foreign source')
                soap(AV, 'SetAVTransportURI', {'InstanceID': 0, 'CurrentURI': base + '/foreign.wav',
                                             'CurrentURIMetaData': metadata}, fault=True)
                check(state_is('radio', 'stop'), 'automatic sender update stole globally stopped radio')
                av('Stop')  # Explicit fresh-Cast boundary clears local override.

                check(state_is('radio', 'stop'), 'UPnP Stop restarted globally stopped radio')
                client.command('playid ' + ids[0])
                wait_radio(ids[0])
                play_foreign()
                client.command('playid ' + ids[1])
                wait_radio(ids[1])
                soap(AV, 'SetAVTransportURI', {'InstanceID': 0, 'CurrentURI': base + '/foreign.wav',
                                             'CurrentURIMetaData': metadata}, fault=True)
                check(state_is('radio', 'play', ids[1]), 'automatic sender update stole selected radio')
                av('Stop')
                play_foreign()
                av('Stop')
                wait_radio(ids[1])
                check('radiod_session' not in client.fields('status'), 'MPD selection retained session')
                check([e['Id'] for e in records(client.command('playlistinfo'))] == ids, 'radio IDs changed after takeover')
                check(not fixture_errors, 'fixture errors: %r' % fixture_errors)
                print('UPnP offline end-to-end: PASS')
            except BaseException:
                if log_path.exists():
                    print('--- radiod daemon log (temporary fixture, loopback URLs only) ---', flush=True)
                    print(log_path.read_text(errors='replace')[-20000:], flush=True)
                raise
            finally:
                for service, sid in subscriptions:
                    try:
                        status, _, _ = request('UNSUBSCRIBE', '/' + service + '/event', headers={'SID': sid})
                        check(status == 200, 'GENA UNSUBSCRIBE failed')
                    except (OSError, TimeoutError):
                        pass  # Daemon failure/deadline must not obstruct bounded cleanup.
                STOP.set()
                if client:
                    client.close()
                if process and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=4)  # includes bounded SSDP departure drain
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=1)
                if server:
                    server.shutdown()
                    server.server_close()
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old_handler)


if __name__ == '__main__':
    main()
