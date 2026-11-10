# Bluetooth receiver input (trusted-LAN v1)

radiod accepts compressed Bluetooth audio relayed by an ESPHome ESP32 receiver.
The ESP is the A2DP sink; radiod decodes standard RTP/SBC or RFC3640 RTP/AAC-LC
and owns the existing ALSA output and dB-aware hardware Master. There is no second
software-volume stage, adaptive input-buffer target, or separate MPD process.

## Configuration

Disabled by default. Use your own deployment addresses; these are documentation
addresses, not an operating installation:

```yaml
bluetooth:
  enabled: true
  listen: 0.0.0.0:8092
  rtp_listen: 0.0.0.0:5004
  advertise_address: 192.0.2.10
  allowed_networks: [192.0.2.0/24]
  allowed_devices: [receiver-1]
  buffer_ms: 500
  lease_ms: 6000
  suspend_ms: 2500
```

`buffer_ms` is a static RTP jitter reserve (50–5000 ms), applied on the next stream
start. Small decode/output queues are separate; neither radio's existing buffer
nor every output queue is changed to 500 ms. The value is not total Bluetooth,
network, decoder and sound-card latency. Other Bluetooth configuration changes
require restart. `advertise_address` must be reachable by the receiver; binding a
Pod address is not sufficient if the sender uses a Kubernetes Service.

Control uses TCP/WebSocket `/v1/receivers`, subprotocol `radiod.receiver.v1`.
Media uses a fixed configured UDP port, bound only for an accepted active input.
Expose both via a Service. Preserve source addresses, for example with a separate
receiver Service using `externalTrafficPolicy: Local` and an actual local endpoint;
do not silently change an existing MPD Service's traffic policy. No multicast,
UPnP, SSDP bridge rule, dedicated LAN attachment or `.4` address is required.

## Scope and safety

**Authentication/TLS are deliberately out of scope.** This endpoint is for a trusted
LAN only, not the Internet. Network/device allowlists, source IP/PT/SSRC checks,
versioning and generation IDs prevent mistakes and stale input, not a malicious
LAN peer. Never describe these identifiers as credentials. No pairing secrets or
Bluetooth link keys are carried in logs or examples.

Control messages/frames are bounded to 16 KiB, connections/rates/outboxes and pending
commands are bounded. The firmware's smaller documented message limit can be used.
Malformed versions, stale sessions/sequences and arbitrary media URLs are rejected.
RTP headers, extensions, padding, source address, dynamic payload type and SSRC are
validated before feeding the bounded decoder pipeline. Missing input cannot become
an indefinitely successful Playing state: real decoded/output data is required,
and starvation, lease expiry and decoder errors terminate or pause according to
source ownership. Teardown joins its worker and must reach NULL before reuse.

## Protocol v1

All device messages start with `v`, `type`, `device_id`, `boot_id`, monotonic `seq`.
After the server's `welcome`, include `control_session_id`; stream-scoped messages
also include `stream_id`. The server similarly numbers messages and echoes boot and
control session. Reconnect gets a fresh session and requires a complete snapshot.
This does not automatically reconnect or play a bonded phone.

The canonical ESPHome handshake is:

```json
{"v":1,"type":"hello","device_id":"receiver-1","boot_id":"boot-1","seq":1,"name":"Receiver","capabilities":{"codecs":["sbc","aac-lc"],"transport_control":true,"absolute_volume":true}}
```

`welcome` returns `heartbeat_interval_ms`, `lease_timeout_ms` (defaults 2000/6000),
current volume/mute and buffer settings. `heartbeat` is acknowledged. The server
also accepts explicit `firmware`, top-level `codecs`, and individual boolean
`play`, `pause`, `metadata` capabilities; those use explicit `volume_synced`
acknowledgments of a previously sent current-volume state. This compatibility
form is tested alongside the canonical firmware handshake.

A phone snapshot, independent of whether music has started:

```json
{"v":1,"type":"state_snapshot","device_id":"receiver-1","boot_id":"boot-1","control_session_id":"server-session","seq":2,"peer":{"connected":true,"peer_id":"AA:BB:CC:DD:EE:FF","display_name":"My phone","name_pending":false},"bluetooth":{"link_state":"connected","audio_state":"suspended","player_state":"unknown"},"capabilities":{"transport_control":true,"absolute_volume":false}}
```

`peer: null` or `peer.connected: false` removes the phone. Names are bounded display
metadata, never keys. Late name updates retain the same song ID; AVRCP state is
`unknown` unless actually observed. Capability changes may be sent in snapshots.
Hello or an online ESP without a phone does not create a Bluetooth song.

`stream_offer` carries the actual negotiated audio format. Example AAC:

```json
{"v":1,"type":"stream_offer","device_id":"receiver-1","boot_id":"boot-1","control_session_id":"server-session","seq":3,"stream_id":"stream-1","audio":{"codec":"aac-lc","sample_rate":44100,"channels":2,"rtp":{"encoding_name":"MPEG4-GENERIC","payload_type":97,"ssrc":123456,"clock_rate":44100,"fmtp":{"mode":"AAC-hbr","config":"1210","size_length":13,"index_length":3,"index_delta_length":3}}}}
```

SBC uses `codec: sbc`, encoding `SBC`, actual sample rate/channels, and no AAC fmtp.
AAC-LC stereo is limited to 44100/config1210 or 48000/config1190. Payload numbers
96/97 are negotiated conventions, not automatic codec identification. Complete
AAC access units use RFC3640 AU headers; SBC uses its standard media header.
A new stream ID and SSRC are required after format change or a stopped generation.
No RTP may be sent until `stream_accept` supplies destination and applied buffer.

Server `command` actions include play/pause/stop with an ID and peer/stream scope.
`command_result` has `status: accepted|rejected|unsupported` (a boolean `accepted`
alias is supported). Acceptance is not proof that the phone changed playback;
subsequent snapshots provide observed state. Rejections and five-second pending
command expiry are reported without blocking local MPD commands.

`player_state` supplies authoritative `revision`, `source`, `desired_transport`,
`volume_percent`, `muted`, current `stream_id`, and `selected_receiver_id` /
`selected_peer_id`. Volume authority follows that selected, synchronized peer,
including while paused without RTP. A request with a nonempty `stream_id` must
match the accepted stream; otherwise it must carry the exact selected `peer_id`.
Selection grants no audio permission: changing volume while paused never resumes
playback. Requests carry an ID, `origin: phone` and `absolute_127`; an authoritative
state with `ack_id` follows.
Policy denials return `accepted: false` without killing a valid connection. The
canonical firmware must apply the initial radio volume before forwarding phone
requests; the server also requires that it sent the still-current baseline.
Neither a cached initial phone value nor an inactive peer may raise radio gain.
A delayed `volume_synced` acknowledgment for an older value is a normal race during
slider movement: return the current state without disconnecting, muting, resetting
initial synchronization or reopening audio. Unknown ACK revisions are similarly
nonfatal but cannot grant initial permission. Wrong session/boot/sequence envelopes
remain invalid. Losing the control transport is not a request to set volume zero.

`receiver_report` is batched at most once per second with received/last sequence,
lost/late/duplicates, jitter, actual decoded/output counts, applied versus configured
buffer and command diagnostics. Successful UDP send alone is not receipt.

## MPD and ownership

A connected phone gets one available `Bluetooth · <name>` entry even when paused.
Bluetooth entries are grouped first in the live queue and catalogue. This is a
stable partition: song IDs, current selection and relative radio ordering do not
change, and the configured `radio` playlist remains radio-only. Its stable ID
survives name changes and a same-boot control reconnect within lease.
Disconnect/lease expiry removes it. Radio playlist contents/IDs remain unchanged;
Bluetooth is available in the live database and queue, never persisted as a radio
URL. Delete/clear can hide an entry until an actual Bluetooth reconnect or explicit
add; snapshots/control reconnect do not silently reinsert it.

Exactly one source owns output. Automatic Bluetooth offers can replace normal radio
only when observed audio is started, not an active UPnP/other Bluetooth source.
Explicit local selection wins. Local Pause/Stop immediately stops and flushes local
output and sends asynchronous AVRCP best effort; no phone acknowledgment is needed.
Old packets/offers cannot override that decision. Selecting Bluetooth explicitly
can request a fresh stream. A phone pause after automatic takeover returns to the
saved radio wish after the fixed suspend debounce; an explicitly selected Bluetooth
source remains paused. Connection alone never starts phone music.

Hardware volume/mute stay global; 0–100 maps to the existing dB-aware ALSA control.
AVRCP absolute volume maps to/from 0–127 with quantization and no echo loop. Firmware
must report unsupported phone behavior honestly; sample attenuation on the phone
plus independent hardware gain is a fallback, not synchronized absolute volume.

## Validation

```sh
cargo test --locked
cargo build --locked
python3 tests/receiver.py
RADIOD_REQUIRE_RTP_CODECS=1 cargo test --locked rtp::tests
```

The real-binary stdlib integration uses null output only, strict and firmware wire
forms, real AAC RTP, packet filters, MPD phone presence/metadata, local overrides,
reconnect/lease and volume rules. SBC/AAC decoder fixtures are mandatory in the
native container/CI. No test script silently starts production playback. Long
hardware duration, actual AVRCP/phone behavior and audible output remain separate
acceptance steps; a passing synthetic test does not certify those.
