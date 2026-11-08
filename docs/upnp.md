# UPnP/DLNA audio renderer PoC

This is an optional **UPnP AV MediaRenderer v1**, not a MediaServer, Google Cast
receiver or DLNA-certified product. It uses GUPnP 1.6 for SSDP, the description
server, SOAP and GENA. A small native C bridge owns its dedicated GLib loop;
Rust's shared Core remains the only owner of playback, volume and source policy.

## Configuration and network

UPnP is disabled by default. Supply installation-specific settings in your own
configuration, not a station catalogue or host address in the product defaults:

```yaml
upnp:
  enabled: true
  bind_address: 192.0.2.10  # documentation address: replace with your actual LAN IP
  interface: net1         # optional; must match the actual LAN interface
  port: 8091
  friendly_name: radiod
  allowed_networks: [192.0.2.0/24]  # replace with your trusted LAN
  prepared_timeout_seconds: 15
  failure_timeout_seconds: 10
```

`bind_address` must already exist; radiod does not configure host interfaces or
allocate IPs. The device UUID is generated once beside the state file and reused.
It is unrelated to station IDs. UPnP settings require a process restart. The HTTP
port defaults to 8091; tests allocate their own port to avoid GSSDP fixed-port
claim/TIME_WAIT conflicts during repeated native runs.

Network requirements:

- SSDP IPv4 multicast **239.255.255.250 UDP/1900**, plus unicast replies.
- Incoming TCP on the configured description/control/event port.
- Outgoing TCP to the phone's audio server and event callback listener.
- The advertised LOCATION must contain the actual reachable LAN address.

A normal Kubernetes Service does not transport LAN SSDP multicast into the Pod.
A separate LAN attachment, for example Multus with a bridge and a reserved static
address, can supply that interface. Keep the primary CNI/default route and existing
MPD Service unchanged. Check real TCP return paths: adding a connected LAN route
can affect responses to Service traffic if its original source is preserved.
Do not enable host networking or weaken namespace policy merely to expose UDP.
Secondary interfaces may not be covered by the primary CNI's NetworkPolicies.

## Implemented services

- **AVTransport:1:** URI and DIDL metadata, Play at speed 1, Pause, Stop, transport,
  media, position, capability, settings and available-action queries.
- **RenderingControl:1:** Master volume and mute, state queries and LastChange.
- **ConnectionManager:1:** supported input protocol information and a single
  connection ID 0.
- **SSDP:** device/service discovery, availability announcements and UUID identity.
- **GENA:** subscription, initial state and actual-content-change notifications.

MP3, AAC, audio MP4, WAV (`audio/wav` and `audio/x-wav`) and Ogg input MIME types
are advertised for the installed GStreamer plugins. Live position/duration are
reported as zero; seeking and next/previous are not advertised. Preset selection
is rejected explicitly. This is a deliberately limited audio-renderer PoC, not a
complete implementation of every optional UPnP AV operation.

## Source ownership and return to radio

- Only one foreign controller/session at a time, identified by HTTP peer address.
  Another controller cannot replace or manipulate the active session. MPD remains
  the local/global control path.
- SetAVTransportURI preserves the prior desired radio selection and playing/paused
  state, stops that engine input and prepares one temporary queue item.
- Play starts the external input. Same-URI metadata updates and redundant Play do
  not reconnect a playing input. Pause retains the session; silence is not its end.
- External Stop or EOS returns to the prior radio. A previously stopped/paused
  radio stays stopped/paused. A deleted return target never selects another sender.
- A prepared-but-never-started session expires. Source failures follow bounded
  retry/backoff and return after the configured continuous-failure grace, not
  indefinite dead-source ownership. Confirmed engine playback clears that clock;
  repeated SOAP Play commands and retry attempts alone do not. Backoff resets only
  after the separate stable-play window.
- MPD Stop suppresses radio return. Explicit MPD station selection ends the foreign
  source and starts the new radio target. Automated metadata from that former
  owner cannot immediately steal the radio back: an explicit UPnP Stop boundary
  is required before it can start a new cast (the AriaCast startup sequence does so).
- The temporary source appears in the active MPD queue/current song with its actual
  title/artist, but never in the configured `radio` playlist or persisted sessions.
- Volume is global and capped. A takeover never applies source metadata as a volume
  change or restores an older, possibly louder level. UPnP volume edits preserve
  mute; an explicit MPD volume edit unmutes. MPD reports effective volume zero while
  muted, whereas UPnP exposes the retained volume and separate mute state.

Foreign sessions are never restored on restart. Only desired radio state and the
existing global volume/mute are persisted. SIGTERM/SIGINT permit GUPnP cleanup and
SSDP departure before process exit without overwriting desired radio intent.

## Initial security bounds

This is a **trusted-LAN, unauthenticated** protocol endpoint. Do not publish it on
the Internet or treat it as an authenticated media service.

- Mutating controllers and media source addresses must match `allowed_networks`.
- For this LAN-capture PoC media URIs require HTTP(S) and a **numeric IP host** in
  that allowlist. DNS names, file URIs, cluster-only hosts and arbitrary public
  Internet media URLs are not accepted through UPnP. Configured radio URLs retain
  their existing separate behavior.
- Limits: URI 4096 bytes, DIDL metadata 8192 bytes, title/artist fields 1024 bytes,
  and HTTP request body 32 KiB. Unsupported/malformed actions return SOAP faults.
- GENA callback preflight accepts bounded numeric IPv4 HTTP destinations reachable
  on the bound interface's network segment. This is not the Rust CIDR rule and does
  not constrain callback HTTP redirects; it is **not comprehensive SSRF isolation**.
  Discovery, descriptions and ordinary state queries are visible to reachable LAN
  clients. The protocol is not a substitute for network access control.
- Raw tokenized source URLs and unrestricted native debug text are not logged.

## AriaCast compatibility target

The reviewed Android sender uses a numeric phone IPv4 URL on TCP/8091, serving
indefinite 48 kHz stereo S16LE WAV with unknown RIFF/data lengths. Its DIDL declares
`audio/x-wav` even when HTTP says `audio/wav`, so both aliases are supported.
Startup is Stop -> SetAVTransportURI -> Play. Title/artist changes use same-URI
SetAVTransportURI -> Play without Stop; these must remain idempotent for audio.
AriaCast does not currently subscribe to DLNA GENA events or send DLNA Pause when
its source app pauses. It may keep a silent stream open: that must not be taken as
a disconnect. Android capture restrictions are independent of this renderer.

Source behavior was reviewed at [AriaCast Android revision 65e75d4](https://github.com/AriaCast/AriaCast-app/commit/65e75d47bf3669052eefc18f6031a7a3a8fba6e7).
This is source inspection, not a claim that the user's exact phone/app combination
has already passed a real audible test.

## Tests

```sh
cargo test --locked
cargo build --locked
python3 tests/upnp.py
# Same tests against a different actual binary:
RADIOD_BINARY=/path/to/radiod python3 tests/upnp.py
```

The stdlib-only integration suite starts the actual renderer with null output,
loopback HTTP audio and GENA listeners. It checks description/SCPDs, standard SOAP
queries/faults, initial and changed events, ownership, indefinite AriaCast-shaped
WAV playback, metadata changes without reconnect, mute/volume, takeover/return,
EOS, errors, timeouts and persistence. Existing audio and metadata regressions run
alongside it. The image build must pass these tests in its actual native runtime.

Separate installation acceptance must verify LAN multicast delivery, advertised
address/source identity, HTTP/callback reachability, preserved MPD Service routing,
restart/CNI cleanup, and finally the actual Android audio capture and listening.
