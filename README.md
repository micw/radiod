# radiod — MPD-compatible radio and live receiver

Rust + embedded GStreamer, an MPD-compatible TCP endpoint and direct ALSA mixer
volume. No MPD daemon, music database or web UI. An optional GUPnP-based audio
MediaRenderer PoC supports temporary LAN streams and controlled radio return;
see [UPnP setup, policy and limitations](docs/upnp.md). It is disabled by default.
An optional [ESP32 Bluetooth receiver input](docs/bluetooth.md) adds WebSocket
control, SBC/AAC RTP decoding and live phone entries in MPD, with a static 500 ms
Bluetooth jitter buffer. It does not require UPnP, multicast or a separate LAN IP.
Authentication is out of scope for this trusted-LAN input; it is disabled by default.
The first client target is MPDroid; MAFA / M.A.L.P. are later interoperability tests.

## Configuration belongs to the installation

**radiod ships no station catalogue or family/host configuration.** The tracked
[example](examples/radiod.yaml) has an empty station list and generic audio devices.
Copy it to your private `radiod.yaml` (gitignored), or mount your own configuration.
The container image does not include any operating station list.

```sh
cp examples/radiod.yaml radiod.yaml
# Edit your devices, safe volume limits, listen address and station list.
cargo run -- --config radiod.yaml --check-config
```

Add your own stations using `name` and an HTTP(S) `url`, for example:

```yaml
stations:
  - name: My station
    url: https://radio.example.org/live
```

That reserved example domain is documentation, not a working station. URLs must
be unique; display names need not be. No manually assigned IDs: internal MPD IDs
are allocated automatically and preserved on reload for unchanged URLs. The list
order is the initial queue order. Alternate URLs per station are not implemented.
Limits: 1 MiB configuration, 1024 stations, 256 bytes per name, 4096 bytes per URL,
and timeout settings between 1 and 3600 seconds.

### Live reload and persistence

The configuration path is reread every second, following it afresh. Atomic file
replacement and Kubernetes ConfigMap symlink swaps are detected. Do not mount an
updating ConfigMap using `subPath`: Kubernetes does not propagate updates there.
Projection of an updated ConfigMap can itself take time.

- Invalid configuration retains the last accepted config and logs a rejection;
  an invalid initial config fails startup.
- Rename/reorder does not restart audio. A changed list resets the mutable MPD
  queue to configured order and preserves the selected URL if still present.
- Removing/editing the active URL stops playback; never select another station.
- Listen address and audio/mixer devices require restart. Retry settings and the
  volume ceiling can reload.
- Persistent JSON stores selected URL, volume and desired playing intent.
  Default `autostart: false` loads selection/volume but remains stopped.
  Opt-in `autostart: true` resumes a saved playing station only if it still exists.
- Pause disconnects a live stream and keeps its station; resume reconnects.
  No timeshift or persistence of temporary phone sessions.

The default bind is `127.0.0.1:6600`. To use a tablet, explicitly bind a reachable
LAN address (or `0.0.0.0` inside a restricted container). **No authentication/TLS**
is implemented. Restrict the MPD port to a trusted network; never expose it publicly.
Only configured URLs can be added through MPD.

## Playback and hardware volume

GStreamer performs URL input, decoding, parsed-audio buffering and ALSA output.
Internet radio keeps a one-second target buffer and 256 KiB byte limit, avoiding
guesses based on bursty download throughput. UPnP LAN capture instead uses a
200 ms target, 32 KiB byte limit and 60 ms ALSA sink buffer; returning to radio
restores its normal buffering. These are receiver targets, not a guarantee of
end-to-end Android/video synchronization.
HTTP radio is not automatically a GStreamer live source. Non-live buffering uses
the standard pause/refill/resume policy; live status is derived from `NoPreroll`,
not from a pipeline latency query (a synchronous sink affects that query too).
Buffering messages are coalesced into a desired state and reconciled against
current/pending pipeline state, including recovery from a late asynchronous pause.
Playing notifications confirm the actual settled state rather than trusting a
potentially historical bus message.

Volume controls the configured ALSA `Master` directly, not software gain in the
stream. Mixer/device names are configurable. Missing mixer means a clear failure,
not silent software fallback. Percentages use an MPD-compatible dB-aware hardware
curve (raw hardware mapping only when valid dB metadata is unavailable). Volume
always permits the full protocol range 0–100; there is no `max_volume` setting.
Stored percentages are reused as-is, not migrated from the former raw curve.
Before upgrading from that curve, lower the stored volume explicitly: the same
number can otherwise become louder. The example's 25% initial value is **not a
guarantee of safe acoustic output**. Verify actual card/control/gain before listening.
UPnP RenderingControl and MPD volume commands both change the hardware directly,
after the audio buffer; changing the phone's source audio is a different path.
Do not let MPD and radiod own the same sound card simultaneously.

Network/decoder failures and unexpected live-stream EOS retry the same selected
station indefinitely: 1, 2, 4, 8, 16, 30 seconds, then every 30 seconds. The cap is
configurable, not a total retry deadline. Reset backoff after 30 seconds of stable
playback. Stop cancels retries; explicit selection starts a fresh attempt.
Connection/buffering stalls have a finite watchdog. Switching sources stops the
old pipeline and flushes its bus before reuse.

MPD's standard state is `play` while buffering, `stop` while reconnecting. Diagnose
using `radiod_state`, `radiod_retry` and `radiod_error`; standard `error` is emitted
as well. Safe typed errors include HTTP status when available, without raw URL or
upstream debug text. Logs include actual playback/buffering/retry transitions.
`radiod_persistence_error` is separate: a disk error must not stop healthy audio.

Stream metadata maps to MPD `Name` (station), `Title` (ICY StreamTitle) and
`Artist` when separately supplied. Actual tag changes, explicit empty tags and
source resets advance the queue version so clients such as MPDroid invalidate
cached queue entries through `plchanges`. Identical repeated tags do not advance
it; station IDs/order are unchanged. No fabricated Artist/Album tags are used to
hide client UI elements. MPDroid's stream-list URL subtitle is client-side behavior;
no sender icons or album-art commands are implemented.

## Native runtime compatibility

The supported image uses Debian Trixie/GStreamer 1.26 and **libsoup 3.8.0**.
Some HTTP/2 redirects fail in older libsoup releases, reproducing identically in
radiod and direct `gst-launch`; this is not an AAC decoder or sender-specific
buffering rule. See the [upstream report](https://gitlab.gnome.org/GNOME/libsoup/-/issues/464)
and [GStreamer discussion](https://discourse.gstreamer.org/t/should-redirects-work-for-http-2-streams/4930).
Updating only the distribution base to libsoup 3.6 did not resolve the reproduced
failure; using the unmodified 3.8.0 library did. No fixed CDN edge URL or blanket
TLS-verification bypass is used.

The Dockerfile downloads the pinned upstream release with SHA-256 verification,
builds it in a separate stage and verifies which library the release tests load.
Its exact source archive and license accompany the image under
`/usr/share/radiod/third-party/`; build dependencies do not enter the runtime image.
Keep this pin updated for upstream security releases. Native host builds need a
compatible HTTP stack too; host success is not a substitute for image testing.

## Build and tests

Native build prerequisites: Rust/Cargo, a C compiler, pkg-config, GStreamer,
ALSA and GUPnP 1.6 development files. Runtime tests require GStreamer base/good/bad/libav plugins for AAC/ICY and
WAV, plus Python 3. Follow the Dockerfile for the supported runtime/toolchain.

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --locked
python3 tests/smoke.py
python3 tests/streaming.py
python3 tests/metadata.py
python3 tests/upnp.py
python3 tests/receiver.py
docker build -t radiod:test .
```

Normal tests are **offline**: generated WAV and a small synthetic AAC fixture over
loopback HTTP, including ICY titles, burst delivery, repeated starts/switches,
reload, stop/retry and volume limits. No public station catalogue is embedded.
`RADIOD_BINARY` can point the Python tests to a different binary. The Docker build
runs these suites against its actual release binary/native runtime as a non-root
user; a successful image build depends on those tests, not just the builder host.
Null output simulates mixer volume and never accesses the actual sound device.
These tests do not prove audible ALSA output or MPDroid interoperability.

Live/network tests are opt-in against an explicit operating configuration:

```sh
python3 tests/live_stations.py --config /path/to/private.yaml \
  --image radiod:test --rounds 2 --max-start-seconds 5 \
  --output /path/to/private-report.json
```

This starts a separate null-output process/container, not the installed service.
It measures actual `radiod_state=play` plus a stable-play hold for every station;
invalid/unavailable URLs fail rather than being treated as decoder successes.
Start-time budgets are acceptance checks, not a promise about an unavailable
provider or an arbitrary Internet connection. The Docker test uses Linux host
networking for its private loopback MPD port.

## MPD scope

Stored playlist `radio`, current queue/song, play/playid, pause/stop, explicit
next/previous, volume, queue edits, command lists and idle. No arbitrary URL player
or general music database. Duplicate queue entries are not supported. Nonzero
repeat/random/single/consume and disabling the sole output are rejected explicitly.
Idle conservatively announces multiple subsystems; statistics durations remain
placeholders. Limits: 32 connections, 8 KiB per command, 256 commands per list.

## Container operation and image builds

Mount your own config at `/config/radiod.yaml` and writable state at `/state`.
For client-only testing add `--null-output`. For published host ports the config
must explicitly bind `0.0.0.0:6600` inside the container. The image runs as UID/GID
65532; provide narrowly scoped sound-device access and matching permissions for
ALSA. Do not solve permissions with blanket privileged access. A read-only root
filesystem with a writable `/tmp` is supported; set `GST_REGISTRY` and
`XDG_CACHE_HOME` there if needed.

GitHub Actions tests pushes to `master`, then publishes linux/amd64 images:
`ghcr.io/micw/radiod:master` and `ghcr.io/micw/radiod:sha-<full-commit>`.
Version tags `v*` publish their corresponding image tag; PRs only run tests.
Publication uses the repository's `GITHUB_TOKEN`, not a stored personal token.
Deploy a verified digest/commit tag for reproducibility, not only a moving tag.
A local Docker image is not automatically in a Kubernetes node's containerd;
use a reachable registry and deliberately configured pull policy/credentials.
This repository's build pipeline does not itself modify a cluster.
