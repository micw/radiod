//! Bounded receiver directory and asynchronous control policy. No socket I/O here.
use super::*;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
};

#[derive(Default)]
pub(super) struct Bluetooth {
    connections: HashMap<u64, Connection>,
    next: u64,
    pub(super) radio: Option<RadioReturn>,
    active: Option<u64>,
    manual: bool,
    suspend: Option<Instant>,
    published: u64,
    report: Option<Instant>,
}
struct Connection {
    ip: IpAddr,
    device: String,
    boot: String,
    session: String,
    input: u64,
    output: u64,
    deadline: Instant,
    closed: bool,
    snapshot: bool,
    peer: String,
    song: Option<u32>,
    capabilities: Value,
    stream: Option<String>,
    ssrc: Option<u32>,
    synced: bool,
    blocked: bool,
    // Wire compatibility is not authentication; legacy firmware gates cached phone values.
    implicit_sync_compat: bool,
    volume_states: VecDeque<(u64, u8, bool)>,
    observed_audio: String,
    observed_player: String,
    requests: VecDeque<String>,
    pending: HashMap<String, Instant>,
    command_error: Option<String>,
    streams: VecDeque<(String, u32)>,
    outbox: VecDeque<Value>,
    rate_start: Instant,
    rate: u32,
}
fn text(v: &Value, key: &str, max: usize) -> Result<String> {
    let s = v[key]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing {key}"))?;
    anyhow::ensure!(
        !s.is_empty() && s.len() <= max && !s.chars().any(|c| c.is_control()),
        "invalid {key}"
    );
    Ok(s.into())
}
impl Connection {
    fn send(&mut self, mut value: Value) {
        // A slow client cannot accumulate unbounded memory. Expire instead of
        // dropping commands and pretending that delivery succeeded.
        if self.outbox.len() >= 64 {
            self.closed = true;
            self.deadline = Instant::now();
            return;
        }
        if value["type"] == "player_state"
            && let (Some(rev), Some(volume), Some(muted)) = (
                value["revision"].as_u64(),
                value["volume_percent"].as_u64(),
                value["muted"].as_bool(),
            )
        {
            if self.volume_states.len() == 16 {
                self.volume_states.pop_front();
            }
            self.volume_states.push_back((rev, volume as u8, muted));
        }
        self.output += 1;
        value["v"] = json!(1);
        value["control_session_id"] = json!(self.session);
        value["boot_id"] = json!(self.boot);
        value["seq"] = json!(self.output);
        self.outbox.push_back(value);
    }
}
impl Core {
    pub fn bluetooth_open(&mut self, peer: IpAddr) -> Result<u64> {
        let cfg = &self.config.bluetooth;
        anyhow::ensure!(
            cfg.enabled && cfg.allowed_networks.iter().any(|n| n.contains(&peer)),
            "receiver address not allowed"
        );
        anyhow::ensure!(
            self.bluetooth.connections.len() < 32,
            "receiver registry full"
        );
        self.bluetooth.next = self
            .bluetooth
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("connection IDs exhausted"))?;
        let id = self.bluetooth.next;
        self.bluetooth.connections.insert(
            id,
            Connection {
                ip: peer,
                device: String::new(),
                boot: String::new(),
                session: format!(
                    "{}-{id}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                ),
                input: 0,
                output: 0,
                deadline: Instant::now() + Duration::from_millis(cfg.lease_ms),
                closed: false,
                snapshot: false,
                peer: String::new(),
                song: None,
                capabilities: Value::Null,
                stream: None,
                ssrc: None,
                synced: false,
                blocked: false,
                implicit_sync_compat: false,
                volume_states: VecDeque::new(),
                observed_audio: "stopped".into(),
                observed_player: "unknown".into(),
                requests: VecDeque::new(),
                streams: VecDeque::new(),
                outbox: VecDeque::new(),
                pending: HashMap::new(),
                command_error: None,
                rate_start: Instant::now(),
                rate: 0,
            },
        );
        Ok(id)
    }
    pub fn bluetooth_drain(&mut self, connection: u64) -> Vec<Value> {
        self.bluetooth
            .connections
            .get_mut(&connection)
            .map(|c| c.outbox.drain(..).collect())
            .unwrap_or_default()
    }
    pub fn bluetooth_alive(&self, connection: u64) -> bool {
        self.bluetooth
            .connections
            .get(&connection)
            .is_some_and(|c| !c.closed && Instant::now() < c.deadline)
    }
    pub fn bluetooth_closed(&mut self, connection: u64) {
        if let Some(c) = self.bluetooth.connections.get_mut(&connection) {
            c.closed = true;
        }
    }
    pub fn bluetooth_message(&mut self, connection: u64, mut message: Value) -> Result<()> {
        anyhow::ensure!(
            serde_json::to_vec(&message)?.len() <= 16384,
            "message too large"
        );
        let mut c = self
            .bluetooth
            .connections
            .remove(&connection)
            .ok_or_else(|| anyhow::anyhow!("unknown connection"))?;
        // Normalize the consolidated firmware's v1 aliases at the API boundary.
        // Strict peers retain the explicit volume_synced handshake.
        let legacy_hello = message["type"] == "hello"
            && message["capabilities"].get("transport_control").is_some();
        if legacy_hello {
            if message.get("firmware").is_none() {
                message["firmware"] = message.get("name").cloned().unwrap_or(json!("receiver-v1"));
            }
            if message.get("codecs").is_none() {
                message["codecs"] = message["capabilities"]["codecs"].clone();
            }
        }
        if message["capabilities"].get("transport_control").is_some() {
            let transport = message["capabilities"]["transport_control"].clone();
            message["capabilities"]["play"] = transport.clone();
            message["capabilities"]["pause"] = transport;
            if message["capabilities"].get("metadata").is_none() {
                message["capabilities"]["metadata"] = json!(true);
            }
        }
        if (message["type"] == "state_snapshot" || message["type"] == "state_changed")
            && message["peer"].is_null()
        {
            message["peer"] = json!({"connected":false});
        }
        if message["type"] == "command_result" && message.get("accepted").is_none() {
            match message["status"].as_str() {
                Some("accepted") => message["accepted"] = json!(true),
                Some("rejected" | "unsupported") => message["accepted"] = json!(false),
                _ => (),
            }
        }
        let result = self.bluetooth_receive(connection, &mut c, &message);
        if legacy_hello && result.is_ok() {
            c.implicit_sync_compat = true;
        }
        self.bluetooth.connections.insert(connection, c);
        result
    }
    fn bluetooth_receive(&mut self, id: u64, c: &mut Connection, m: &Value) -> Result<()> {
        anyhow::ensure!(
            !c.closed && Instant::now() < c.deadline,
            "connection expired"
        );
        if c.rate_start.elapsed() >= Duration::from_secs(1) {
            c.rate = 0;
            c.rate_start = Instant::now();
        }
        c.rate += 1;
        anyhow::ensure!(c.rate <= 64, "receiver message rate exceeded");
        anyhow::ensure!(m["v"] == 1, "unsupported protocol version");
        let kind = text(m, "type", 32)?;
        let seq = m["seq"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("invalid sequence"))?;
        if c.device.is_empty() {
            anyhow::ensure!(kind == "hello" && seq == 1, "hello required");
            let device = text(m, "device_id", 64)?;
            anyhow::ensure!(
                device
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
                "invalid device ID"
            );
            let boot = text(m, "boot_id", 64)?;
            text(m, "firmware", 256)?;
            anyhow::ensure!(
                self.config.bluetooth.allowed_devices.is_empty()
                    || self.config.bluetooth.allowed_devices.contains(&device),
                "device not allowed"
            );
            for key in ["play", "pause", "absolute_volume", "metadata"] {
                anyhow::ensure!(m["capabilities"][key].is_boolean(), "invalid capabilities");
            }
            let codecs = m["codecs"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("missing codecs"))?;
            anyhow::ensure!(
                !codecs.is_empty()
                    && codecs.len() <= 2
                    && codecs.iter().all(|c| c == "sbc" || c == "aac-lc"),
                "invalid codecs"
            );
            let previous = self
                .bluetooth
                .connections
                .iter()
                .find(|(_, other)| other.device == device)
                .map(|(id, _)| *id);
            if let Some(old) = previous {
                let other = &self.bluetooth.connections[&old];
                anyhow::ensure!(other.closed, "device already connected");
                let mut other = self.bluetooth.connections.remove(&old).unwrap();
                if other.boot == boot {
                    c.blocked = other.blocked;
                    c.song = other.song.take();
                    c.peer = other.peer.clone();
                    c.ssrc = other.ssrc;
                    c.streams = other.streams;
                    if self.bluetooth.active == Some(old) {
                        self.bluetooth.active = Some(id);
                        let _ = self.audio.stop();
                        self.desired = false;
                        self.state = "pause".into();
                    }
                } else {
                    self.bluetooth_remove(old, other);
                }
            }
            c.device = device;
            c.boot = boot;
            c.input = seq;
            c.capabilities = m["capabilities"].clone();
            c.send(json!({"type":"welcome","heartbeat_ms":2000u64.min(self.config.bluetooth.lease_ms / 3),"heartbeat_interval_ms":2000u64.min(self.config.bluetooth.lease_ms / 3),"lease_timeout_ms":self.config.bluetooth.lease_ms,"lease_ms":self.config.bluetooth.lease_ms,"buffer_ms":self.config.bluetooth.buffer_ms,"volume_percent":self.volume,"muted":self.muted}));
            c.send(self.bluetooth_state_for(id, c, None));
            return Ok(());
        }
        anyhow::ensure!(
            m["control_session_id"] == c.session && m["boot_id"] == c.boot && seq > c.input,
            "stale receiver envelope"
        );
        anyhow::ensure!(
            kind == "state_snapshot" || c.snapshot || kind == "heartbeat",
            "full snapshot required"
        );
        c.input = seq;
        c.deadline = Instant::now() + Duration::from_millis(self.config.bluetooth.lease_ms);
        match kind.as_str() {
            "heartbeat" => c.send(json!({"type":"heartbeat_ack"})),
            "state_snapshot" | "state_changed" => {
                let capabilities_changed = if let Some(capabilities) = m.get("capabilities") {
                    for key in ["play", "pause", "absolute_volume", "metadata"] {
                        anyhow::ensure!(capabilities[key].is_boolean(), "invalid capabilities");
                    }
                    &c.capabilities != capabilities
                } else {
                    false
                };
                let connected = m["peer"]["connected"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("missing peer presence"))?;
                let audio = text(&m["bluetooth"], "audio_state", 16)?;
                let player = text(&m["bluetooth"], "player_state", 16)?;
                anyhow::ensure!(
                    ["started", "suspended", "stopped"].contains(&audio.as_str())
                        && ["playing", "paused", "stopped", "unknown"].contains(&player.as_str()),
                    "invalid Bluetooth state"
                );
                anyhow::ensure!(
                    if connected {
                        m["bluetooth"]["link_state"] == "connected"
                    } else {
                        m["bluetooth"]["link_state"].as_str().is_some_and(|s| {
                            ["disconnected", "connecting", "disconnecting"].contains(&s)
                        })
                    },
                    "inconsistent link state"
                );
                if connected {
                    let peer = text(&m["peer"], "peer_id", 64)?;
                    anyhow::ensure!(
                        peer.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
                        "invalid peer ID"
                    );
                    anyhow::ensure!(
                        m["peer"]["name_pending"].is_boolean(),
                        "missing name status"
                    );
                    let name = m["peer"]["display_name"].as_str().unwrap_or("");
                    anyhow::ensure!(
                        name.len() <= 256 && !name.chars().any(|c| c.is_control()),
                        "invalid phone name"
                    );
                    if c.song.is_some() && c.peer != peer {
                        self.bluetooth_disconnect(id, c)?;
                    }
                    c.peer = peer;
                    let name = if name.is_empty() {
                        format!("Bluetooth · {}", c.device)
                    } else {
                        format!("Bluetooth · {name}")
                    };
                    if let Some(song) = c.song {
                        if let Some(s) = self.songs.iter_mut().find(|s| s.id == song)
                            && s.station.name != name
                        {
                            s.station.name = name;
                            self.playlist_version += 1;
                            self.changed();
                        }
                    } else {
                        let song = self.next_id;
                        self.next_id = self
                            .next_id
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("song IDs exhausted"))?;
                        self.songs.push(Song {
                            id: song,
                            station: Station {
                                name,
                                url: format!("bluetooth://{}/{}", c.device, c.peer),
                            },
                            temporary: true,
                            source: SourceKind::Bluetooth,
                        });
                        self.queue.push(song);
                        self.prioritize_bluetooth();
                        c.song = Some(song);
                        self.playlist_version += 1;
                        self.changed();
                    }
                    if self.bluetooth.active == Some(id) {
                        if audio != "started" || player == "paused" || player == "stopped" {
                            self.audio.stop()?;
                            c.stream = None;
                            self.desired = false;
                            self.state = "pause".into();
                            self.changed();
                            if !self.bluetooth.manual {
                                self.bluetooth.suspend.get_or_insert(
                                    Instant::now()
                                        + Duration::from_millis(self.config.bluetooth.suspend_ms),
                                );
                            }
                        } else {
                            self.bluetooth.suspend = None;
                        }
                        if let Some(meta) = m.get("metadata") {
                            // The consolidated firmware snapshots its current peer's
                            // metadata without a stream field. Accept that only in its
                            // negotiated dialect/current accepted stream; an explicitly
                            // wrong generation is never allowed to overwrite metadata.
                            if c.stream.is_some()
                                && (c.stream.as_deref() == m["stream_id"].as_str()
                                    || (c.implicit_sync_compat && m.get("stream_id").is_none()))
                            {
                                let mut tags = Vec::new();
                                for key in ["title", "artist"] {
                                    let value = meta.get(key).and_then(Value::as_str);
                                    anyhow::ensure!(
                                        value.is_none_or(|s| s.len() <= 1024),
                                        "metadata too large"
                                    );
                                    tags.push(value.map(str::to_owned));
                                }
                                self.receive_metadata(tags[0].clone(), tags[1].clone());
                            }
                        }
                    }
                } else {
                    self.bluetooth_disconnect(id, c)?;
                }
                if capabilities_changed {
                    c.capabilities = m["capabilities"].clone();
                    c.synced = false;
                    c.send(self.bluetooth_state_for(id, c, None));
                }
                c.observed_audio = audio;
                c.observed_player = player;
                c.snapshot = true;
            }
            "stream_offer" => {
                let stream = text(m, "stream_id", 64)?;
                let format: crate::rtp::RtpFormat = serde_json::from_value(m["audio"].clone())?;
                format.validate()?;
                let ssrc = m["audio"]["rtp"]["ssrc"].as_u64().unwrap_or(0) as u32;
                let eligible = c.song.is_some_and(|song| self.queue.contains(&song))
                    && ((self.bluetooth.active == Some(id)
                        && self.bluetooth.manual
                        && self.desired)
                        || (c.observed_audio == "started"
                            && c.observed_player != "paused"
                            && c.observed_player != "stopped"))
                    && !c.blocked
                    && self.external.is_none()
                    && (self.bluetooth.active.is_none() || self.bluetooth.active == Some(id));
                if !eligible
                    || c.streams
                        .iter()
                        .any(|(old_stream, old_ssrc)| old_stream == &stream || *old_ssrc == ssrc)
                    || c.ssrc == Some(ssrc)
                {
                    c.send(json!({"type":"stream_reject","stream_id":stream,"reason":"busy, overridden, or stale generation"}));
                } else {
                    self.bluetooth_capture(id, c.song.unwrap(), false)?;
                    c.stream = None;
                    match self.audio.play_rtp(
                        &format,
                        self.config.bluetooth.rtp_listen,
                        c.ip,
                        self.config.bluetooth.buffer_ms,
                    ) {
                        Ok(port) => {
                            if c.streams.len() == 64 {
                                c.streams.pop_front();
                            }
                            c.streams.push_back((stream.clone(), ssrc));
                            c.stream = Some(stream.clone());
                            c.ssrc = Some(ssrc);
                            self.desired = true;
                            self.state = "buffering".into();
                            self.started = Some(Instant::now());
                            self.retry = None;
                            self.changed();
                            c.send(json!({"type":"stream_accept","stream_id":stream,"destination":{"host":self.config.bluetooth.advertise_address,"port":port},"buffer_ms":self.config.bluetooth.buffer_ms}));
                        }
                        Err(_) => {
                            c.send(json!({"type":"stream_reject","stream_id":stream,"reason":"audio unavailable"}));
                            self.bluetooth_failure("Bluetooth audio unavailable".into());
                        }
                    }
                }
            }
            "stream_end" => {
                anyhow::ensure!(
                    self.bluetooth.active == Some(id)
                        && c.stream.as_deref() == m["stream_id"].as_str()
                        && c.stream.is_some(),
                    "stale stream"
                );
                self.audio.stop()?;
                c.stream = None;
                self.desired = false;
                self.state = "pause".into();
                self.changed();
                if !self.bluetooth.manual {
                    self.bluetooth.suspend = Some(
                        Instant::now() + Duration::from_millis(self.config.bluetooth.suspend_ms),
                    );
                }
            }
            "volume_synced" => {
                anyhow::ensure!(
                    c.volume_states
                        .iter()
                        .any(
                            |(revision, volume, muted)| Some(*revision) == m["revision"].as_u64()
                                && *volume == self.volume
                                && *muted == self.muted
                        ),
                    "stale volume revision"
                );
                c.synced = true;
            }
            "volume_request" => {
                let request = text(m, "id", 64)?;
                anyhow::ensure!(!c.requests.contains(&request), "duplicate volume request");
                let v = m["absolute_127"]
                    .as_u64()
                    .filter(|v| *v <= 127)
                    .ok_or_else(|| anyhow::anyhow!("invalid volume"))?;
                let stream = text(m, "stream_id", 64)?;
                anyhow::ensure!(m["origin"] == "phone", "invalid volume origin");
                let allowed = self.bluetooth.active == Some(id)
                    && (c.synced
                        || (c.implicit_sync_compat
                            && c.snapshot
                            && c.volume_states
                                .iter()
                                .any(|(_, v, m)| *v == self.volume && *m == self.muted)))
                    && c.capabilities["absolute_volume"] == true
                    && c.stream.as_deref() == Some(stream.as_str());
                if !allowed {
                    let mut state = self.bluetooth_state_for(id, c, Some(request));
                    state["accepted"] = json!(false);
                    state["reason"] = json!("volume owner not synchronized");
                    c.send(state);
                    return Ok(());
                }
                self.upnp_set_volume(((v * 100 + 63) / 127) as u8)?;
                if c.requests.len() == 64 {
                    c.requests.pop_front();
                }
                c.requests.push_back(request.clone());
                c.send(self.bluetooth_state_for(id, c, Some(request)));
            }
            "command_result" => {
                let command = text(m, "id", 64)?;
                anyhow::ensure!(m["accepted"].is_boolean(), "invalid command result");
                anyhow::ensure!(
                    c.pending.remove(&command).is_some(),
                    "unknown or expired command"
                );
                c.command_error = if m["accepted"] == true {
                    None
                } else {
                    Some("receiver rejected command".into())
                };
                self.changed();
            }
            _ => anyhow::bail!("unknown receiver message"),
        }
        Ok(())
    }
    pub(super) fn bluetooth_state(&self, ack: Option<String>) -> Value {
        let active_stream = self
            .bluetooth
            .active
            .and_then(|id| self.bluetooth.connections.get(&id))
            .and_then(|c| c.stream.clone());
        json!({"type":"player_state","revision":self.revision,"source":if self.bluetooth_selected(){"bluetooth"}else if self.external.is_some(){"upnp"}else{"radio"},"desired_transport":if self.state == "pause" {"pause"}else if self.desired {"play"}else{"stop"},"volume_percent":self.volume,"muted":self.muted,"ack_id":ack,"active_stream_id":active_stream,"stream_id":active_stream})
    }
    pub(super) fn bluetooth_block_present(&mut self) {
        for c in self
            .bluetooth
            .connections
            .values_mut()
            .filter(|c| c.song.is_some())
        {
            c.blocked = true;
        }
    }
    pub(super) fn bluetooth_failure(&mut self, error: String) {
        let _ = self.audio.stop();
        if let Some(c) = self
            .bluetooth
            .active
            .and_then(|id| self.bluetooth.connections.get_mut(&id))
        {
            c.stream = None;
        }
        self.desired = false;
        self.retry = None;
        self.started = None;
        self.stable = None;
        if self.bluetooth.manual {
            self.state = "pause".into();
        } else {
            let _ = self.bluetooth_return(true);
        }
        self.error = Some(error);
        self.changed();
    }
    fn bluetooth_state_for(&self, id: u64, c: &Connection, ack: Option<String>) -> Value {
        let mut state = self.bluetooth_state(ack);
        if self.bluetooth.active == Some(id) {
            state["stream_id"] = json!(c.stream);
            state["active_stream_id"] = json!(c.stream);
        }
        state
    }
    pub fn bluetooth_selected(&self) -> bool {
        self.bluetooth.active.is_some()
    }
    pub(super) fn bluetooth_capture(&mut self, id: u64, song: u32, manual: bool) -> Result<()> {
        if self.bluetooth.active != Some(id) {
            self.audio.stop()?;
            self.bluetooth.radio = Some(RadioReturn {
                current: self.current,
                desired: self.desired,
                paused: self.state == "pause",
            });
            self.bluetooth.active = Some(id);
            self.bluetooth.manual = manual;
            self.current = Some(song);
            self.set_metadata(None, None);
            self.retry = None;
            self.stable = None;
            self.error = None;
        } else if manual {
            self.bluetooth.manual = true;
            if self.current != Some(song) {
                self.set_metadata(None, None);
                self.current = Some(song);
            }
        }
        self.bluetooth.suspend = None;
        Ok(())
    }
    pub(super) fn bluetooth_command(&mut self, action: &str) {
        if let Some(c) = self
            .bluetooth
            .active
            .and_then(|id| self.bluetooth.connections.get_mut(&id))
        {
            if c.pending.len() >= 32 {
                c.command_error = Some("receiver command queue full".into());
                return;
            }
            let command = format!("cmd-{}-{}", self.revision, c.output + 1);
            c.pending
                .insert(command.clone(), Instant::now() + Duration::from_secs(5));
            c.send(json!({"type":"command","id":command,"peer_id":c.peer,"stream_id":c.stream,"action":action}));
        }
    }
    pub(super) fn bluetooth_select(&mut self, song: u32) -> Result<()> {
        let id = self
            .bluetooth
            .connections
            .iter()
            .find(|(_, c)| c.song == Some(song) && !c.closed && c.snapshot)
            .map(|(id, _)| *id)
            .ok_or_else(|| anyhow::anyhow!("receiver unavailable"))?;
        if self.external.is_some() {
            self.finish_external(false)?;
        }
        if self.bluetooth.active.is_some() && self.bluetooth.active != Some(id) {
            self.bluetooth_override()?;
        }
        self.bluetooth_capture(id, song, true)?;
        self.bluetooth_play()
    }
    pub(super) fn bluetooth_play(&mut self) -> Result<()> {
        let selectable = self
            .bluetooth
            .active
            .and_then(|id| self.bluetooth.connections.get(&id))
            .is_some_and(|c| {
                !c.closed
                    && c.snapshot
                    && c.song.is_some_and(|song| {
                        self.current == Some(song) && self.queue.contains(&song)
                    })
            });
        anyhow::ensure!(
            selectable,
            "Bluetooth source hidden or unavailable; add and select it first"
        );
        if let Some(c) = self
            .bluetooth
            .active
            .and_then(|id| self.bluetooth.connections.get_mut(&id))
        {
            c.blocked = false;
        }
        self.bluetooth.manual = true;
        self.bluetooth.suspend = None;
        self.audio.stop()?;
        if let Some(c) = self
            .bluetooth
            .active
            .and_then(|id| self.bluetooth.connections.get_mut(&id))
        {
            c.stream = None;
        }
        self.desired = true;
        self.state = "buffering".into();
        self.started = Some(Instant::now());
        self.retry = None;
        self.changed();
        self.bluetooth_command("play");
        self.persist();
        Ok(())
    }
    pub(super) fn bluetooth_stop(&mut self, pause: bool) -> Result<()> {
        if let Some(radio) = &mut self.bluetooth.radio {
            radio.desired = false;
            radio.paused = false;
        }
        if let Some(c) = self
            .bluetooth
            .active
            .and_then(|id| self.bluetooth.connections.get_mut(&id))
        {
            c.blocked = true;
        }
        self.bluetooth.manual = true;
        self.bluetooth.suspend = None;
        self.desired = false;
        self.retry = None;
        self.started = None;
        self.stable = None;
        self.state = if pause { "pause" } else { "stop" }.into();
        self.changed();
        self.bluetooth_command(if pause { "pause" } else { "stop" });
        if let Some(c) = self
            .bluetooth
            .active
            .and_then(|id| self.bluetooth.connections.get_mut(&id))
        {
            c.stream = None;
        }
        let result = self.audio.stop();
        self.persist();
        result
    }
    pub(super) fn bluetooth_override(&mut self) -> Result<()> {
        self.bluetooth_block_present();
        self.bluetooth_return(false)
    }
    pub(super) fn bluetooth_return(&mut self, restore: bool) -> Result<()> {
        if self.bluetooth.active.is_none() {
            return Ok(());
        }
        self.bluetooth_command("stop");
        let stopped = self.audio.stop();
        self.bluetooth.active = None;
        self.bluetooth.suspend = None;
        let radio = self.bluetooth.radio.take();
        self.current = radio
            .as_ref()
            .and_then(|r| r.current)
            .filter(|id| self.queue.contains(id));
        self.desired = false;
        self.retry = None;
        self.started = None;
        self.stable = None;
        self.state = if restore && radio.as_ref().is_some_and(|r| r.paused) {
            "pause"
        } else {
            "stop"
        }
        .into();
        self.set_metadata(None, None);
        self.changed();
        if restore && stopped.is_ok() && radio.is_some_and(|r| r.desired) && self.current.is_some()
        {
            self.start_source()?;
        }
        self.persist();
        stopped
    }
    fn bluetooth_disconnect(&mut self, id: u64, c: &mut Connection) -> Result<()> {
        if self.bluetooth.active == Some(id) {
            self.bluetooth_return(true)?;
        }
        if let Some(song) = c.song.take() {
            self.songs.retain(|s| s.id != song);
            self.queue.retain(|s| *s != song);
            self.playlist_version += 1;
            self.changed();
        }
        c.peer.clear();
        c.pending.clear();
        c.requests.clear();
        c.streams.clear();
        c.command_error = None;
        c.stream = None;
        c.ssrc = None;
        c.synced = false;
        c.volume_states.clear();
        c.observed_audio = "stopped".into();
        c.observed_player = "unknown".into();
        c.blocked = false;
        Ok(())
    }
    fn bluetooth_remove(&mut self, id: u64, mut c: Connection) {
        let _ = self.bluetooth_disconnect(id, &mut c);
    }
    pub(super) fn bluetooth_tick(&mut self) {
        let mut expired_commands = false;
        for c in self.bluetooth.connections.values_mut() {
            let before = c.pending.len();
            c.pending.retain(|_, deadline| Instant::now() < *deadline);
            if before != c.pending.len() {
                c.command_error = Some("receiver command timed out".into());
                expired_commands = true;
            }
        }
        if expired_commands {
            self.changed();
        }
        if self
            .bluetooth
            .report
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.bluetooth.report = Some(Instant::now());
            let stats = self.audio.rtp_stats();
            if let Some(c) = self
                .bluetooth
                .active
                .and_then(|id| self.bluetooth.connections.get_mut(&id))
                && !c.closed
                && let Some(mut report) = stats
            {
                report["type"] = json!("receiver_report");
                report["stream_id"] = json!(c.stream);
                report["configured_buffer_ms"] = json!(self.config.bluetooth.buffer_ms);
                report["pending_commands"] = json!(c.pending.len());
                report["command_error"] = json!(c.command_error);
                c.send(report);
            }
        }
        let expired: Vec<_> = self
            .bluetooth
            .connections
            .iter()
            .filter(|(_, c)| Instant::now() >= c.deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(c) = self.bluetooth.connections.remove(&id) {
                self.bluetooth_remove(id, c);
            }
        }
        if self.bluetooth.suspend.is_some_and(|t| Instant::now() >= t) {
            let _ = self.bluetooth_return(true);
        }
        if self.bluetooth.published != self.revision {
            let state = self.bluetooth_state(None);
            for c in self
                .bluetooth
                .connections
                .values_mut()
                .filter(|c| !c.device.is_empty() && !c.closed)
            {
                let mut state = state.clone();
                state["pending_commands"] = json!(c.pending.len());
                state["command_error"] = json!(c.command_error);
                c.send(state);
            }
            self.bluetooth.published = self.revision;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Core, u64) {
        let mut cfg = Config::default();
        cfg.bluetooth.enabled = true;
        cfg.bluetooth.allowed_networks = vec!["127.0.0.0/8".parse().unwrap()];
        cfg.bluetooth.advertise_address = Some("127.0.0.1".parse().unwrap());
        cfg.stations = vec![Station {
            name: "Radio".into(),
            url: "http://127.0.0.1:1/radio".into(),
        }];
        let path = std::env::temp_dir().join(format!(
            "radiod-bt-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut c = Core::new(
            cfg,
            Audio::new("default", "default", "Master", true).unwrap(),
            path,
        )
        .unwrap();
        let id = c.bluetooth_open("127.0.0.1".parse().unwrap()).unwrap();
        hello(&mut c, id);
        (c, id)
    }
    fn hello(c: &mut Core, id: u64) {
        c.bluetooth_message(id,json!({"v":1,"type":"hello","seq":1,"device_id":"receiver","boot_id":"boot","firmware":"test","capabilities":{"play":true,"pause":true,"absolute_volume":true,"metadata":false},"codecs":["sbc"]})).unwrap();
    }
    fn message(c: &mut Core, id: u64, mut m: Value) -> Result<()> {
        let con = &c.bluetooth.connections[&id];
        m["v"] = json!(1);
        m["seq"] = json!(con.input + 1);
        m["boot_id"] = json!(con.boot);
        m["control_session_id"] = json!(con.session);
        c.bluetooth_message(id, m)
    }
    fn snapshot(name: &str, connected: bool) -> Value {
        json!({"type":"state_snapshot","peer":{"connected":connected,"peer_id":"phone","display_name":name,"name_pending":false},"bluetooth":{"link_state":if connected {"connected"}else{"disconnected"},"audio_state":"suspended","player_state":"unknown"}})
    }
    #[test]
    fn occupied_rtp_port_rejects_offer_and_preserves_return_policy() {
        let held = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        for manual in [false, true] {
            let (mut c, id) = fixture();
            c.config.bluetooth.rtp_listen = held.local_addr().unwrap();
            let mut state = snapshot("Phone", true);
            state["bluetooth"]["audio_state"] = json!("started");
            message(&mut c, id, state).unwrap();
            let radio = c.current;
            c.desired = true;
            let song = c.bluetooth.connections[&id].song.unwrap();
            if manual {
                c.select(song).unwrap();
            }
            message(&mut c,id,json!({"type":"stream_offer","stream_id":"new","audio":{"codec":"sbc","sample_rate":44100,"channels":2,"rtp":{"encoding_name":"SBC","payload_type":96,"ssrc":77,"clock_rate":44100}}})).unwrap();
            assert!(
                c.bluetooth_drain(id)
                    .iter()
                    .any(|m| m["type"] == "stream_reject")
            );
            if manual {
                assert_eq!(c.current, Some(song));
                assert_eq!(c.state, "pause");
                assert!(c.bluetooth.radio.as_ref().unwrap().desired);
            } else {
                assert_eq!(c.current, radio);
                assert!(c.desired);
                assert!(!c.bluetooth_selected());
            }
        }
    }
    #[test]
    fn technical_failure_preserves_radio_return_intent() {
        for manual in [false, true] {
            let (mut c, id) = fixture();
            message(&mut c, id, snapshot("Phone", true)).unwrap();
            let radio = c.current;
            c.desired = true;
            let song = c.bluetooth.connections[&id].song.unwrap();
            c.bluetooth_capture(id, song, manual).unwrap();
            c.failed("RTP starvation".into());
            assert!(!c.bluetooth.connections[&id].blocked);
            if manual {
                assert_eq!(c.current, Some(song));
                assert_eq!(c.state, "pause");
                assert!(c.bluetooth.radio.as_ref().unwrap().desired);
            } else {
                assert_eq!(c.current, radio);
                assert!(c.desired);
                assert!(!c.bluetooth_selected());
            }
        }
    }
    #[test]
    fn unrelated_disconnect_cannot_release_another_peers_stop_fence() {
        let (mut c, a) = fixture();
        message(&mut c, a, snapshot("A", true)).unwrap();
        let b = c.bluetooth_open("127.0.0.1".parse().unwrap()).unwrap();
        c.bluetooth_message(b,json!({"v":1,"type":"hello","seq":1,"device_id":"other","boot_id":"boot","firmware":"test","capabilities":{"play":true,"pause":true,"absolute_volume":true,"metadata":false},"codecs":["sbc"]})).unwrap();
        message(&mut c, b, snapshot("B", true)).unwrap();
        let song = c.bluetooth.connections[&a].song.unwrap();
        c.select(song).unwrap();
        c.stop(false).unwrap();
        message(&mut c, b, snapshot("", false)).unwrap();
        assert!(c.bluetooth.connections[&a].blocked);
        c.bluetooth_closed(a);
        let fresh = c.bluetooth_open("127.0.0.1".parse().unwrap()).unwrap();
        hello(&mut c, fresh);
        assert!(c.bluetooth.connections[&fresh].blocked);
    }
    #[test]
    fn active_stream_aliases_survive_volume_ack_and_capability_response() {
        let (mut c, id) = fixture();
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        let song = c.bluetooth.connections[&id].song.unwrap();
        c.select(song).unwrap();
        {
            let con = c.bluetooth.connections.get_mut(&id).unwrap();
            con.stream = Some("accepted-stream".into());
            con.synced = true;
        }
        message(&mut c,id,json!({"type":"volume_request","id":"vol","stream_id":"accepted-stream","origin":"phone","absolute_127":64})).unwrap();
        assert!(c.bluetooth_drain(id).iter().any(|m| m["ack_id"] == "vol"
            && m["stream_id"] == "accepted-stream"
            && m["active_stream_id"] == "accepted-stream"));
        let mut state = snapshot("Phone", true);
        state["bluetooth"]["audio_state"] = json!("started");
        state["capabilities"] =
            json!({"play":true,"pause":true,"absolute_volume":true,"metadata":true});
        message(&mut c, id, state).unwrap();
        assert!(
            c.bluetooth_drain(id)
                .iter()
                .any(|m| m["type"] == "player_state" && m["stream_id"] == "accepted-stream")
        );
    }
    #[test]
    fn presence_name_idempotence_hide_and_reconnect() {
        let (mut c, id) = fixture();
        assert_eq!(c.songs.len(), 1); // hello is not phone presence
        message(&mut c, id, snapshot("", true)).unwrap();
        let song = c.bluetooth.connections[&id].song.unwrap();
        let version = c.playlist_version;
        message(&mut c, id, snapshot("", true)).unwrap();
        assert_eq!(c.playlist_version, version);
        message(&mut c, id, snapshot("Actual phone", true)).unwrap();
        assert_eq!(c.bluetooth.connections[&id].song, Some(song));
        assert_eq!(
            c.song(song).unwrap().station.name,
            "Bluetooth · Actual phone"
        );
        c.queue.retain(|id| *id != song);
        c.queue_updated();
        message(&mut c, id, snapshot("Actual phone", true)).unwrap();
        assert!(!c.queue.contains(&song));
        let old_session = c.bluetooth.connections[&id].session.clone();
        c.bluetooth_closed(id);
        let new = c.bluetooth_open("127.0.0.1".parse().unwrap()).unwrap();
        hello(&mut c, new);
        assert_ne!(c.bluetooth.connections[&new].session, old_session);
        message(&mut c, new, snapshot("Actual phone", true)).unwrap();
        assert_eq!(c.bluetooth.connections[&new].song, Some(song));
        assert!(!c.queue.contains(&song)); // control reconnect is not a new BT presence
        assert_eq!(c.songs.len(), 2);
        message(&mut c, new, snapshot("", false)).unwrap();
        assert!(c.song(song).is_none());
    }
    #[test]
    fn consolidated_firmware_aliases_and_implicit_sync_still_require_active_stream() {
        let (mut c, old) = fixture();
        c.bluetooth_closed(old);
        let id = c.bluetooth_open("127.0.0.1".parse().unwrap()).unwrap();
        c.bluetooth_message(id,json!({"v":1,"type":"hello","seq":1,"device_id":"receiver","boot_id":"boot","name":"receiver","capabilities":{"codecs":["sbc","aac-lc"],"transport_control":true,"absolute_volume":true}})).unwrap();
        assert!(c.bluetooth.connections[&id].implicit_sync_compat);
        let welcome = c.bluetooth_drain(id);
        assert!(welcome.iter().any(|m| m["type"] == "welcome"
            && m["heartbeat_interval_ms"] == 2000
            && m["lease_timeout_ms"] == 6000));
        let mut state = snapshot("Phone", true);
        state["peer"]["peer_id"] = json!("AA:BB:CC:DD:EE:FF");
        state["capabilities"] = json!({"transport_control":true,"absolute_volume":true});
        message(&mut c, id, state).unwrap();
        let old_volume = c.volume;
        message(&mut c,id,json!({"type":"volume_request","id":"inactive","stream_id":"stream","origin":"phone","absolute_127":127})).unwrap();
        assert_eq!(c.volume, old_volume);
        let song = c.bluetooth.connections[&id].song.unwrap();
        c.select(song).unwrap();
        // Model accepted audio ownership without opening a UDP socket in this policy test.
        c.bluetooth.connections.get_mut(&id).unwrap().stream = Some("stream".into());
        message(&mut c,id,json!({"type":"volume_request","id":"active","stream_id":"stream","origin":"phone","absolute_127":64})).unwrap();
        assert_eq!(c.volume, 50);
        let command = c.bluetooth.connections[&id]
            .pending
            .keys()
            .next()
            .unwrap()
            .clone();
        message(
            &mut c,
            id,
            json!({"type":"command_result","id":command,"status":"unsupported"}),
        )
        .unwrap();
        message(&mut c,id,json!({"type":"state_snapshot","peer":null,"bluetooth":{"link_state":"disconnecting","audio_state":"suspended","player_state":"unknown"}})).unwrap();
        assert!(c.song(song).is_none());
    }
    #[test]
    fn volume_sync_tolerates_metadata_revisions_but_not_changed_volume() {
        let (mut c, id) = fixture();
        let revision = c.bluetooth.connections[&id].volume_states.back().unwrap().0;
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        message(&mut c, id, snapshot("Actual name", true)).unwrap();
        let mut playing = snapshot("Actual name", true);
        playing["bluetooth"]["audio_state"] = json!("started");
        playing["bluetooth"]["player_state"] = json!("playing");
        message(&mut c, id, playing).unwrap();
        message(
            &mut c,
            id,
            json!({"type":"volume_synced","revision":revision}),
        )
        .unwrap();
        c.set_volume(if c.volume == 100 { 99 } else { 100 })
            .unwrap();
        assert!(
            message(
                &mut c,
                id,
                json!({"type":"volume_synced","revision":revision})
            )
            .is_err()
        );
    }
    #[test]
    fn inactive_volume_request_is_a_policy_response_not_a_disconnect() {
        let (mut c, id) = fixture();
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        let volume = c.volume;
        message(&mut c,id,json!({"type":"volume_request","id":"vol-1","stream_id":"inactive","origin":"phone","absolute_127":127})).unwrap();
        assert_eq!(c.volume, volume);
        assert!(c.bluetooth_alive(id));
        let out = c.bluetooth_drain(id);
        assert!(out.iter().any(|v| v["ack_id"] == "vol-1"
            && v["accepted"] == false
            && v["type"] == "player_state"));
    }
    #[test]
    fn paused_phone_offer_does_not_automatically_start_audio() {
        let (mut c, id) = fixture();
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        message(&mut c,id,json!({"type":"stream_offer","stream_id":"stream","audio":{"codec":"sbc","sample_rate":44100,"channels":2,"rtp":{"encoding_name":"SBC","payload_type":96,"ssrc":1,"clock_rate":44100}}})).unwrap();
        assert!(!c.bluetooth_selected());
        assert!(
            c.bluetooth_drain(id)
                .iter()
                .any(|m| m["type"] == "stream_reject")
        );
    }
    #[test]
    fn lease_duplicate_live_and_stale_envelopes() {
        let (mut c, id) = fixture();
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        let other = c.bluetooth_open("127.0.0.1".parse().unwrap()).unwrap();
        let hello_msg = json!({"v":1,"type":"hello","seq":1,"device_id":"receiver","boot_id":"boot","firmware":"test","capabilities":{"play":true,"pause":true,"absolute_volume":true,"metadata":false},"codecs":["sbc"]});
        assert!(c.bluetooth_message(other, hello_msg).is_err());
        let rev = c.revision;
        assert!(c.bluetooth_message(id,json!({"v":1,"type":"state_changed","seq":99,"boot_id":"old","control_session_id":"old"})).is_err());
        assert_eq!(c.revision, rev);
        c.bluetooth.connections.get_mut(&id).unwrap().deadline = Instant::now();
        c.tick();
        assert_eq!(c.songs.len(), 1);
    }
    #[test]
    fn automatic_suspend_returns_radio_but_manual_suspend_retains_selection() {
        for manual in [false, true] {
            let (mut c, id) = fixture();
            message(&mut c, id, snapshot("Phone", true)).unwrap();
            let radio = c.current;
            let song = c.bluetooth.connections[&id].song.unwrap();
            c.bluetooth_capture(id, song, manual).unwrap();
            message(&mut c, id, snapshot("Phone", true)).unwrap();
            if manual {
                assert!(c.bluetooth.suspend.is_none());
            } else {
                c.bluetooth.suspend = Some(Instant::now());
            }
            c.tick();
            assert_eq!(c.current, if manual { Some(song) } else { radio });
        }
    }
    #[test]
    fn active_upnp_and_local_override_reject_offers_without_binding_audio() {
        let (mut c, id) = fixture();
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        let offer = json!({"type":"stream_offer","stream_id":"stream-1","audio":{"codec":"sbc","sample_rate":44100,"channels":2,"rtp":{"encoding_name":"SBC","payload_type":96,"ssrc":1,"clock_rate":44100}}});
        c.prepare_external("owner", "http://127.0.0.1:1/external", None, None)
            .unwrap();
        let current = c.current;
        message(&mut c, id, offer.clone()).unwrap();
        assert_eq!(c.current, current);
        assert!(
            c.bluetooth_drain(id)
                .iter()
                .any(|m| m["type"] == "stream_reject")
        );
        c.external_stop("owner").unwrap();
        let radio = c.radio_song_ids()[0];
        c.select(radio).unwrap();
        message(&mut c, id, offer).unwrap();
        assert_eq!(c.current, Some(radio));
        assert!(!c.bluetooth_selected());
    }
    #[test]
    fn command_rejection_and_timeout_are_bounded_diagnostics_not_transport_acks() {
        let (mut c, id) = fixture();
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        let song = c.bluetooth.connections[&id].song.unwrap();
        c.select(song).unwrap();
        let command = c.bluetooth.connections[&id]
            .pending
            .keys()
            .next()
            .unwrap()
            .clone();
        message(
            &mut c,
            id,
            json!({"type":"command_result","id":command,"accepted":false}),
        )
        .unwrap();
        assert!(c.desired); // dispatch rejection is not observed phone transport
        assert!(c.bluetooth.connections[&id].command_error.is_some());
        c.stop(true).unwrap();
        for deadline in c
            .bluetooth
            .connections
            .get_mut(&id)
            .unwrap()
            .pending
            .values_mut()
        {
            *deadline = Instant::now();
        }
        c.tick();
        assert_eq!(c.state, "pause");
        assert!(!c.desired);
        assert!(c.bluetooth.connections[&id].pending.is_empty());
        assert_eq!(
            c.bluetooth.connections[&id].command_error.as_deref(),
            Some("receiver command timed out")
        );
        assert!(c.bluetooth_alive(id));
        c.bluetooth_closed(id);
        assert!(!c.bluetooth_alive(id));
    }
    #[test]
    fn local_pause_stops_immediately_and_never_persists_foreign_url() {
        let (mut c, id) = fixture();
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        let song = c.bluetooth.connections[&id].song.unwrap();
        c.select(song).unwrap();
        c.stop(true).unwrap();
        assert_eq!(c.state, "pause");
        assert!(!c.desired);
        assert!(c.retry.is_none());
        message(&mut c, id, snapshot("Phone", true)).unwrap();
        c.tick();
        assert_eq!(c.current, Some(song));
        assert_eq!(c.state, "pause");
        c.save().unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(&c.path).unwrap()).unwrap();
        assert!(!saved.to_string().contains("bluetooth://"));
        assert_eq!(saved["playing"], false);
        message(&mut c, id, snapshot("", false)).unwrap();
        assert!(!c.desired);
    }
}
