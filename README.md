# radiod — radio-only MPD replacement (first PoC)

Rust + embedded GStreamer, an MPD-compatible TCP endpoint and direct ALSA mixer
volume. No MPD daemon, web UI, UPnP or phone-stream takeover in this version.
**Not yet accepted with MPDroid or real ALSA hardware.**

## Build and local test

Native dependencies: Rust/Cargo, pkg-config, GStreamer >= 1.14 development files,
ALSA development files. Runtime needs ALSA, GStreamer base/good/bad/ugly/libav
plugins (actual codec support depends on installed plugins) and CA certificates.
The checked-in lockfile is authoritative; see the Dockerfile for the build toolchain.

```sh
cargo build --locked
cargo test --locked
cargo run -- --config radiod.yaml --check-config
# No sound card access, volume is simulated; useful for client/API tests:
cargo run -- --config radiod.yaml --state state/test.json --null-output
python3 tests/smoke.py
```

For real playback, check the ALSA device and mixer configuration first, then omit
`--null-output`. Do not run against the same card as an active MPD instance.
Volume is set on the hardware mixer **before playback**. Start stopped by default.
The supplied 25% / maximum 70% are conservative software limits, **not a guarantee
of safe acoustic output**. Verify the device and actual hardware gain before a
listening test. No mixer means startup fails; there is no software-volume fallback.
The example uses the previously identified `Generic_1` card ID; verify it on the
actual host instead of assuming that either that ID or numeric card 1 is stable.

## Configuration

[radiod.yaml](radiod.yaml) migrates the existing MPD chart's station list.
The duplicate “Antenne Bayern Lounge” entry is consolidated to the direct
`s5-webradio.antenne.de` URL, leaving 16 stations. Alternate URLs per station are
not implemented in V1; internal MPD IDs still need no user configuration.
Each station needs only `name` and an HTTP(S) `url`. URLs must be unique; names
need not be. The list order is the initial queue order. Limits: 1 MiB config, 1024 stations,
256 bytes per name, 4096 bytes per URL, timeout settings 1–3600 seconds.
Internal MPD song IDs are
allocated automatically and preserved across reloads for unchanged URLs.

The config path is reread every second, following the path afresh: atomic file
replacement and Kubernetes ConfigMap symlink swaps are detected. No ConfigMap
`subPath` mount: Kubernetes does not propagate updates to that type of mount.
Kubernetes itself may take time to project a changed ConfigMap.

- Invalid YAML/unknown fields/invalid values: retain the last accepted config,
  log the rejection. On invalid initial configuration, fail startup.
- Rename or reorder: no stream restart. Changed list resets the mutable MPD queue
  to configured order, preserving the selected URL if still present.
- Active URL removed (including a URL edit): stop, never pick another station.
- Mixer/output device or listen address changes require process restart and cause
  the entire live reload to be rejected. Retry settings and volume ceiling reload.
- Persistent JSON state stores selected URL, volume and desired playing flag.
  Default `autostart: false` loads selection/volume but remains stopped.
  `autostart: true` resumes only if the saved desired state was playing.
- Pause disconnects a live stream and retains its station; resume reconnects.
  There is no timeshift.

`listen` defaults to `127.0.0.1:6600`. For the tablet, explicitly configure a LAN
binding (or `0.0.0.0:6600` inside an isolated container) and restrict access to the
trusted home network. This PoC has **no authentication or TLS**; do not publish it
to the Internet. Only configured stream URLs can be added through MPD.

## Reconnect policy

Network/decoder errors and unexpected live-stream EOS reconnect **the same selected
station indefinitely**. Delays: 1, 2, 4, 8, 16, 30 seconds, then every 30 seconds.
`retry_max_seconds` changes the cap, not a total retry deadline. Reset the backoff
after 30 seconds of stable playback. Connection attempts have a finite timeout.
Stop cancels retries; explicit station selection starts a fresh attempt immediately.
The old GStreamer pipeline is stopped and bus events flushed before a new source.

MPD standard state is `play` while buffering and `stop` while reconnecting (MPD
has no exact matching states). Desired playback intent remains active during retry.
A standard `error` field reports failures. Additional `radiod_state`, `radiod_retry`
and `radiod_error` fields expose more detail; MPDroid may not display extensions.
Upstream error/debug strings are not exposed because they can contain URL tokens.
Persistence failures are reported separately as `radiod_persistence_error`; they
never cause a healthy audio stream to stop. Reload remains accepted in this case,
but changed state may be lost on restart until persistence works again.

## MPD scope and acceptance

First target: **MPDroid**, tested by the user. MAFA / M.A.L.P. are later candidates.
Stored playlist `radio`, current queue/song, play/playid, pause/stop, explicit
next/previous, volume, queue edits, command lists and idle are supported. This is
not a full MPD music database or arbitrary Internet URL player. Duplicate queue
entries are deliberately not supported (station IDs are unique). Nonzero
repeat/random/single/consume options and disabling the sole output are explicitly
rejected, rather than accepted without effect. `idle` conservatively announces
multiple subsystems; statistics durations are currently placeholders. Connections
are limited to 32, command lines to 8 KiB and command lists to 256 commands.

Before replacing the old service, verify on the actual tablet:

1. Connect, show/load `radio`, select stations, play/stop and adjust volume.
2. Observe station names, ICY titles and updates without reconnecting the client.
3. Network loss preserves station, retries with backoff; Stop cancels retry.
4. YAML rename/reorder/rejection/removal behavior and persisted restart state.
5. Explicitly scheduled ALSA listening test with old MPD stopped, low initial gain,
   and a rollback path. No Kubernetes deployment is implied by this implementation.

## Initial station probe

Null-output probes reached playing for the ten non-404 stations, including the
Ogg Radio Paradise stream and Radio Leipzig's AAC stream (despite its MP3-looking
URL). Radio Leipzig exposed a live/buffering transition bug; after the fix it also
reached playing. A regression test covers resuming a previously buffer-paused
pipeline when its source is discovered to be live.

A separate bounded HTTP GET probe returned 404 for the inherited KR New Classics,
KR Klassik Meats Rock, KR Movie, KR Games, KR Lounge Beats and Charivari Lounge
URLs. These six URLs remain in the configuration for explicit user review, not
silently replaced. Network/provider behavior can change; these checks are not a
listening or long-duration stability acceptance.

## Container and Talos mw-home

```sh
docker build -t radiod:poc .
# Offline-ish client test: no sound devices, simulated hardware volume
# Supply a config with listen: 0.0.0.0:6600 for host port access.
docker run --rm -p 127.0.0.1:6600:6600 \
  -v "$PWD/radiod.yaml:/config/radiod.yaml:ro" \
  --tmpfs /state:uid=65532,gid=65532 \
  radiod:poc --config /config/radiod.yaml --state /state/state.json --null-output
```

The image runs as UID/GID 65532. Real ALSA needs deliberately granted device access
and matching permissions, plus writable `/state`. Current host device permissions
may prevent non-root access. Do not solve that with a blanket privileged namespace.
Container audio playback requires an explicit device/security plan before rollout.

Read-only cluster inspection confirmed `homehost` is linux/amd64, Talos 1.14.5;
local talosctl is 1.14.4. This CLI has **no `image import` subcommand**. A local
Docker image is not automatically available to the node's containerd.

Preferred test distribution: push a unique tag to a reachable registry, then use
that exact reference in the separately authorized test workload. GHCR is already
used in the home repository, but radiod repository/push permissions are unverified.
For example, after choosing the registry and authenticating:

```sh
docker tag radiod:poc ghcr.io/micw/radiod:test-UNIQUE
docker push ghcr.io/micw/radiod:test-UNIQUE
# Optional pre-pull (changes node image cache, does not create a Pod):
talosctl \
  --talosconfig /home/mwyraz/git/mw-kubernetes/env/mw-home/talos/clusterconfig/talosconfig \
  -n 192.168.1.9 image --namespace cri pull ghcr.io/micw/radiod:test-UNIQUE
```

Build on amd64 or explicitly cross-build for linux/amd64. `docker buildx` is not
assumed installed. A private registry requires appropriate pull credentials.
Use unique tags or digests; `IfNotPresent` permits a pre-pulled image, whereas
`Always` may still contact the registry. Node caches are node-specific and subject
to garbage collection. A local LAN registry is another option but requires separate
routing/TLS/Talos-registry planning; node `localhost` is not the build host.
### GitHub image builds

The GitHub Actions pipeline tests each master-branch push before publishing
`ghcr.io/micw/radiod:master` and `ghcr.io/micw/radiod:sha-<full-commit>` for linux/amd64.
Version tags `v*` also publish the corresponding image tag. Pull requests run tests
without registry writes. The workflow uses the repository's `GITHUB_TOKEN`, just
like the existing Dockernode build; no manually stored registry token is needed.
For reproducible cluster tests use the commit tag or digest, not the moving `master`.
Ensure the resulting GHCR package is publicly pullable or configure imagePullSecrets.
Publication does not deploy anything. No node cache mutation, shutdown of MPD or
Kubernetes deployment is part of this build step.
