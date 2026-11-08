use crate::{
    audio::{Audio, Event},
    config::{Config, Station},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};
#[derive(Clone, Serialize)]
pub struct Song {
    pub id: u32,
    pub station: Station,
    pub temporary: bool,
}
#[derive(Serialize, Deserialize, Default)]
struct Saved {
    url: Option<String>,
    volume: Option<u8>,
    #[serde(default)]
    playing: bool,
    #[serde(default)]
    muted: bool,
}
struct RadioReturn {
    current: Option<u32>,
    desired: bool,
    paused: bool,
}
struct External {
    id: u64,
    owner: String,
    uri: String,
    title: Option<String>,
    artist: Option<String>,
    radio: RadioReturn,
    prepared: Option<Instant>,
    failed: Option<Instant>,
}
pub struct Core {
    pub config: Config,
    pub songs: Vec<Song>,
    pub queue: Vec<u32>,
    pub current: Option<u32>,
    pub volume: u8,
    pub muted: bool,
    external: Option<External>,
    // Explicit local overrides suppress a phone's automatic metadata recasts.
    // A fresh Stop from that owner is the boundary for a deliberate new cast.
    blocked_external_owner: Option<String>,
    generation: u64,
    pub state: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub error: Option<String>,
    pub persistence_error: Option<String>,
    pub revision: u64,
    pub playlist_version: u64,
    pub desired: bool,
    audio: Audio,
    path: PathBuf,
    next_id: u32,
    retry: Option<Instant>,
    attempt: u32,
    started: Option<Instant>,
    stable: Option<Instant>,
}
impl Core {
    pub fn new(config: Config, mut audio: Audio, path: PathBuf) -> Result<Self> {
        let saved: Saved = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let volume = saved
            .volume
            .unwrap_or(config.initial_volume)
            .min(config.max_volume);
        audio.set_volume(if saved.muted { 0 } else { volume })?;
        let songs: Vec<_> = config
            .stations
            .iter()
            .enumerate()
            .map(|(i, s)| Song {
                id: i as u32 + 1,
                station: s.clone(),
                temporary: false,
            })
            .collect();
        let current = match saved.url {
            Some(url) => songs.iter().find(|s| s.station.url == url).map(|s| s.id),
            None => songs.first().map(|s| s.id),
        };
        let queue = songs.iter().map(|s| s.id).collect();
        let next_id = songs.len() as u32 + 1;
        let autostart = config.autostart && saved.playing && current.is_some();
        let mut c = Self {
            config,
            songs,
            queue,
            current,
            volume,
            muted: saved.muted,
            external: None,
            blocked_external_owner: None,
            generation: 0,
            state: "stop".into(),
            title: None,
            artist: None,
            error: None,
            persistence_error: None,
            revision: 1,
            playlist_version: 1,
            desired: false,
            audio,
            path,
            next_id,
            retry: None,
            attempt: 0,
            started: None,
            stable: None,
        };
        if autostart {
            c.play()?;
        }
        Ok(c)
    }
    pub fn song(&self, id: u32) -> Option<&Song> {
        self.songs.iter().find(|s| s.id == id)
    }
    pub fn changed(&mut self) {
        self.revision += 1;
    }
    pub fn queue_updated(&mut self) {
        if let Some(e) = &mut self.external
            && e.radio.current.is_some_and(|id| !self.queue.contains(&id))
        {
            e.radio.current = None;
            e.radio.desired = false;
            e.radio.paused = false;
        }
        self.playlist_version += 1;
        self.changed();
        self.persist();
    }
    fn set_metadata(&mut self, title: Option<String>, artist: Option<String>) {
        let title = title.filter(|s| !s.is_empty());
        let artist = artist.filter(|s| !s.is_empty());
        if self.title == title && self.artist == artist {
            return;
        }
        self.title = title;
        self.artist = artist;
        // MPD clients cache queue entries by this version, including stream
        // titles. An idle notification alone does not invalidate that cache.
        self.playlist_version += 1;
        self.changed();
    }
    fn receive_metadata(&mut self, title: Option<String>, artist: Option<String>) {
        // Missing fields are partial updates; explicitly empty tags clear them.
        let title = title.or_else(|| self.title.clone());
        let artist = artist.or_else(|| self.artist.clone());
        self.set_metadata(title, artist);
    }
    pub fn save(&self) -> Result<()> {
        let radio_current = self
            .external
            .as_ref()
            .map(|e| e.radio.current)
            .unwrap_or(self.current)
            .filter(|id| self.queue.contains(id) && self.song(*id).is_some_and(|s| !s.temporary));
        let saved = Saved {
            url: radio_current.and_then(|id| self.song(id).map(|s| s.station.url.clone())),
            volume: Some(self.volume),
            playing: self
                .external
                .as_ref()
                .map(|e| e.radio.desired)
                .unwrap_or(self.desired)
                && radio_current.is_some(),
            muted: self.muted,
        };
        if let Some(p) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(p)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&saved)?)?;
        std::fs::rename(tmp, &self.path)?;
        Ok(())
    }
    fn persist(&mut self) {
        let error = self
            .save()
            .err()
            .map(|e| format!("cannot persist radio state: {e}"));
        if error != self.persistence_error {
            if let Some(e) = &error {
                eprintln!("{e}");
            }
            self.persistence_error = error;
            self.changed();
        }
    }
    pub fn play(&mut self) -> Result<()> {
        if let Some(e) = &mut self.external {
            e.failed = None;
            e.prepared = None;
        }
        self.start_source()
    }
    fn start_source(&mut self) -> Result<()> {
        if self.current.is_none() {
            self.current = self.queue.first().copied();
        }
        let Some(url) = self
            .current
            .and_then(|id| self.song(id).map(|s| s.station.url.clone()))
        else {
            return Ok(());
        };
        self.audio.stop()?;
        self.retry = None;
        self.started = Some(Instant::now());
        self.stable = None;
        self.desired = true;
        self.state = "buffering".into();
        self.set_metadata(None, None);
        if let Some(e) = &self.external {
            let (t, a) = (e.title.clone(), e.artist.clone());
            self.set_metadata(t, a);
        }
        self.error = None;
        if let Err(e) = self.audio.play(&url) {
            self.failed(e.to_string());
        }
        self.changed();
        self.persist();
        Ok(())
    }
    pub fn select(&mut self, id: u32) -> Result<()> {
        anyhow::ensure!(self.queue.contains(&id), "unknown song");
        if self.song(id).is_some_and(|s| !s.temporary) && self.external.is_some() {
            self.finish_external(false)?;
        }
        if self.current != Some(id) {
            // Invalidate the former queue entry before changing its ownership.
            self.set_metadata(None, None);
        }
        self.current = Some(id);
        self.attempt = 0;
        self.play()
    }
    pub fn stop(&mut self, pause: bool) -> Result<()> {
        if !pause && self.external.is_some() {
            self.finish_external(false)?;
        }
        if let Some(e) = &mut self.external {
            e.prepared = None;
            e.failed = None;
        }
        self.desired = false;
        self.retry = None;
        self.started = None;
        self.stable = None;
        self.attempt = 0;
        let result = self.audio.stop();
        self.state = if pause { "pause" } else { "stop" }.into();
        self.error = result
            .as_ref()
            .err()
            .map(|_| "audio shutdown failed".into());
        self.changed();
        self.persist();
        result
    }
    pub fn set_volume(&mut self, v: u8) -> Result<()> {
        let v = v.min(self.config.max_volume);
        self.audio.set_volume(v)?;
        self.volume = v;
        self.muted = false;
        self.changed();
        self.persist();
        Ok(())
    }
    fn failed(&mut self, error: String) {
        let _ = self.audio.stop();
        if let Some(e) = &mut self.external {
            e.failed.get_or_insert_with(Instant::now);
        }
        self.state = "reconnecting".into();
        self.error = Some(error);
        self.started = None;
        self.stable = None;
        let seconds = retry_delay(self.attempt, self.config.retry_max_seconds);
        self.attempt = self.attempt.saturating_add(1);
        self.retry = Some(Instant::now() + Duration::from_secs(seconds));
        eprintln!(
            "audio state=reconnecting station_id={:?} retry_seconds={} error={}",
            self.current,
            seconds,
            self.error.as_deref().unwrap_or("unknown")
        );
        self.changed();
    }
    fn buffering(&mut self, percent: u8) {
        if percent < 100 {
            if self.state != "buffering" {
                eprintln!(
                    "audio state=buffering station_id={:?} percent={percent}",
                    self.current
                );
            }
            self.state = "buffering".into();
            self.stable = None;
            self.started.get_or_insert_with(Instant::now);
        }
        // Only actual Playing confirms output has resumed.
        self.changed();
    }
    fn ended(&mut self) {
        if self.external.is_some() {
            let _ = self.finish_external(true);
        } else {
            self.failed("unexpected end of live stream".into());
        }
    }
    fn confirmed_playing(&mut self) {
        if self.state != "play" {
            eprintln!("audio state=playing station_id={:?}", self.current);
        }
        self.state = "play".into();
        self.started = None;
        self.stable.get_or_insert_with(Instant::now);
        self.error = None;
        if let Some(e) = &mut self.external {
            e.failed = None;
        }
        self.changed();
    }
    pub fn tick(&mut self) {
        if self.external.as_ref().is_some_and(|e| {
            e.prepared
                .is_some_and(|t| t.elapsed().as_secs() >= self.config.upnp.prepared_timeout_seconds)
        }) {
            let _ = self.finish_external(true);
            return;
        }
        if self.desired {
            for e in self.audio.poll() {
                match e {
                    Event::Playing => self.confirmed_playing(),
                    Event::Buffering(p) => self.buffering(p),
                    Event::Tags { title, artist } => self.receive_metadata(title, artist),
                    Event::Ended => {
                        self.ended();
                        return;
                    }
                    Event::Error(e) => {
                        self.failed(e);
                        break;
                    }
                }
            }
            if self
                .started
                .is_some_and(|t| t.elapsed().as_secs() >= self.config.connect_timeout_seconds)
            {
                self.failed("connection timeout".into());
            }
            // Process a queued successful recovery before expiring its old
            // failure clock; healthy playback must not be cut off at 10 seconds.
            if self.external.as_ref().is_some_and(|e| {
                e.failed.is_some_and(|t| {
                    t.elapsed().as_secs() >= self.config.upnp.failure_timeout_seconds
                })
            }) {
                let _ = self.finish_external(true);
                return;
            }
            if self.stable.is_some_and(|t| t.elapsed().as_secs() >= 30) {
                self.attempt = 0;
            }
            if self.retry.is_some_and(|t| Instant::now() >= t)
                && let Err(e) = self.start_source()
            {
                self.failed(e.to_string());
            }
        }
    }
    pub fn reload(&mut self, new: Config) -> Result<()> {
        anyhow::ensure!(
            new.listen == self.config.listen
                && new.alsa_device == self.config.alsa_device
                && new.mixer_device == self.config.mixer_device
                && new.mixer_control == self.config.mixer_control
                && new.upnp == self.config.upnp,
            "listen/audio device changes require restart"
        );
        // Build the complete replacement before touching live metadata.
        let old: HashMap<_, _> = self
            .songs
            .iter()
            .filter(|s| !s.temporary)
            .map(|s| (s.station.url.clone(), s.id))
            .collect();
        let mut next_id = self.next_id;
        let mut songs = Vec::new();
        for station in &new.stations {
            let id = match old.get(&station.url) {
                Some(id) => *id,
                None => {
                    let id = next_id;
                    next_id = next_id
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("song ID space exhausted"))?;
                    id
                }
            };
            songs.push(Song {
                id,
                station: station.clone(),
                temporary: false,
            });
        }
        if let Some(ghost) = self.songs.iter().find(|s| s.temporary) {
            songs.push(ghost.clone());
        }
        let removed = self
            .current
            .is_some_and(|id| !songs.iter().any(|s| s.id == id));
        let volume = self.volume.min(new.max_volume);
        // Hardware failures reject the config before changing stations/queue.
        if volume != self.volume {
            self.audio.set_volume(if self.muted { 0 } else { volume })?;
        }
        if removed && let Err(e) = self.audio.stop() {
            let _ = self.audio.set_volume(self.effective_volume());
            return Err(e);
        }
        if new.stations != self.config.stations {
            self.songs = songs;
            self.queue = self.songs.iter().map(|s| s.id).collect();
            self.next_id = next_id;
            self.playlist_version += 1;
        }
        if removed {
            self.current = None;
            self.desired = false;
            self.retry = None;
            self.started = None;
            self.stable = None;
            self.attempt = 0;
            self.state = "stop".into();
            self.title = None;
            self.artist = None;
            self.error = None;
        }
        if let Some(e) = &mut self.external
            && e.radio
                .current
                .is_some_and(|id| !self.songs.iter().any(|s| s.id == id))
        {
            e.radio.current = None;
            e.radio.desired = false;
        }
        self.volume = volume;
        self.config = new;
        self.changed();
        // Persistence is reported separately; disk errors must not partly reject
        // an otherwise accepted config or interrupt a healthy stream.
        self.persist();
        Ok(())
    }
    pub fn radio_song_ids(&self) -> Vec<u32> {
        self.songs
            .iter()
            .filter(|s| !s.temporary)
            .map(|s| s.id)
            .collect()
    }
    pub fn external_session_id(&self) -> Option<u64> {
        self.external.as_ref().map(|e| e.id)
    }
    pub fn external_owner(&self) -> Option<&str> {
        self.external.as_ref().map(|e| e.owner.as_str())
    }
    fn check_owner(&self, owner: &str) -> Result<()> {
        anyhow::ensure!(
            self.external_owner() == Some(owner),
            "external source owner mismatch"
        );
        Ok(())
    }
    pub fn effective_volume(&self) -> u8 {
        if self.muted {
            0
        } else {
            self.volume.min(self.config.max_volume)
        }
    }
    pub fn set_mute(&mut self, muted: bool) -> Result<()> {
        self.audio.set_volume(if muted {
            0
        } else {
            self.volume.min(self.config.max_volume)
        })?;
        self.muted = muted;
        self.changed();
        self.persist();
        Ok(())
    }
    pub fn upnp_set_volume(&mut self, v: u8) -> Result<()> {
        let v = v.min(self.config.max_volume);
        self.audio.set_volume(if self.muted { 0 } else { v })?;
        self.volume = v;
        self.changed();
        self.persist();
        Ok(())
    }
    pub fn prepare_external(
        &mut self,
        owner: &str,
        uri: &str,
        title: Option<String>,
        artist: Option<String>,
    ) -> Result<u64> {
        anyhow::ensure!(
            self.blocked_external_owner.as_deref() != Some(owner),
            "external owner overridden; Stop required before a new cast"
        );
        anyhow::ensure!(
            !owner.is_empty()
                && (uri.starts_with("http://") || uri.starts_with("https://"))
                && !uri.chars().any(|c| c.is_whitespace() || c.is_control()),
            "invalid external source"
        );
        if let Some(e) = &self.external {
            self.check_owner(owner)?;
            if e.uri == uri {
                let id = e.id;
                if let Some(e) = &mut self.external {
                    if title.is_some() {
                        e.title = title.clone();
                    }
                    if artist.is_some() {
                        e.artist = artist.clone();
                    }
                }
                self.receive_metadata(title, artist);
                return Ok(id);
            }
        }
        let next = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("song ID space exhausted"))?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("session ID space exhausted"))?;
        self.audio.stop()?;
        let radio = if let Some(e) = self.external.take() {
            e.radio
        } else {
            RadioReturn {
                current: self.current,
                desired: self.desired,
                paused: self.state == "pause",
            }
        };
        self.songs.retain(|s| !s.temporary);
        self.queue
            .retain(|id| self.songs.iter().any(|s| s.id == *id));
        let id = self.next_id;
        self.next_id = next;
        self.generation = generation;
        self.songs.push(Song {
            id,
            station: Station {
                name: "UPnP".into(),
                url: uri.into(),
            },
            temporary: true,
        });
        self.queue.push(id);
        self.current = Some(id);
        self.desired = false;
        self.retry = None;
        self.started = None;
        self.stable = None;
        self.attempt = 0;
        self.state = "stop".into();
        self.error = None;
        self.external = Some(External {
            id: generation,
            owner: owner.into(),
            uri: uri.into(),
            title: title.clone(),
            artist: artist.clone(),
            radio,
            prepared: Some(Instant::now()),
            failed: None,
        });
        self.set_metadata(title, artist);
        self.playlist_version += 1;
        self.changed();
        self.persist();
        Ok(generation)
    }
    pub fn external_play(&mut self, owner: &str) -> Result<()> {
        self.check_owner(owner)?;
        // AriaCast updates DIDL using SetAVTransportURI(same URL), then Play.
        // Repeated Play must not reconnect audio or reset the failure grace.
        if self.desired {
            return Ok(());
        }
        self.play()
    }
    pub fn external_pause(&mut self, owner: &str) -> Result<()> {
        self.check_owner(owner)?;
        self.stop(true)
    }
    pub fn external_stop(&mut self, owner: &str) -> Result<()> {
        if self.external.is_none() {
            if self.blocked_external_owner.as_deref() == Some(owner) {
                self.blocked_external_owner = None;
            }
            return Ok(());
        }
        self.check_owner(owner)?;
        self.finish_external(true)
    }
    fn finish_external(&mut self, restore: bool) -> Result<()> {
        // Even failed shutdown must relinquish the foreign session and timers;
        // never start a return source on top of an unsettled engine.
        let shutdown = self.audio.stop();
        let Some(e) = self.external.take() else {
            return shutdown;
        };
        if !restore {
            self.blocked_external_owner = Some(e.owner.clone());
        }
        let restore = restore && shutdown.is_ok();
        self.songs.retain(|s| !s.temporary);
        self.queue
            .retain(|id| self.songs.iter().any(|s| s.id == *id));
        self.current = e
            .radio
            .current
            .filter(|id| self.queue.contains(id) && self.song(*id).is_some_and(|s| !s.temporary));
        self.desired = false;
        self.retry = None;
        self.started = None;
        self.stable = None;
        self.attempt = 0;
        self.error = None;
        self.state = if restore && e.radio.paused {
            "pause"
        } else {
            "stop"
        }
        .into();
        self.set_metadata(None, None);
        self.playlist_version += 1;
        self.changed();
        if restore && e.radio.desired && self.current.is_some() {
            self.start_source()?;
        }
        if shutdown.is_err() {
            self.error = Some("audio shutdown failed".into());
        }
        self.persist();
        shutdown
    }
    pub fn retry_seconds(&self) -> Option<u64> {
        self.retry
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs())
    }
}
pub fn retry_delay(attempt: u32, cap: u64) -> u64 {
    (1u64.checked_shl(attempt.min(63)).unwrap_or(u64::MAX)).min(cap)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Core {
        let c = Config {
            stations: vec![
                Station {
                    name: "A".into(),
                    url: "http://127.0.0.1:1/a".into(),
                },
                Station {
                    name: "B".into(),
                    url: "http://127.0.0.1:1/b".into(),
                },
            ],
            ..Config::default()
        };
        let path = std::env::temp_dir().join(format!(
            "radiod-test-{}-{:?}/state.json",
            std::process::id(),
            std::thread::current().id()
        ));
        Core::new(
            c,
            Audio::new("default", "default", "Master", true).unwrap(),
            path,
        )
        .unwrap()
    }
    #[test]
    fn backoff() {
        assert_eq!(
            (0..8).map(|n| retry_delay(n, 30)).collect::<Vec<_>>(),
            vec![1, 2, 4, 8, 16, 30, 30, 30]
        );
    }
    #[test]
    fn configurable_cap() {
        assert_eq!(retry_delay(30, 60), 60);
    }
    #[test]
    fn retry_never_selects_next() {
        let mut c = fixture();
        let id = c.current;
        c.desired = true;
        for _ in 0..20 {
            c.failed("offline".into());
            assert_eq!(c.current, id);
            assert!(c.retry.is_some());
        }
        c.stop(false).unwrap();
        assert!(c.retry.is_none());
        assert!(!c.desired);
    }
    #[test]
    fn reload_preserves_ids_and_stops_removed() {
        let mut c = fixture();
        let id = c.current;
        let mut new = c.config.clone();
        new.stations.reverse();
        new.stations[1].name = "renamed".into();
        c.reload(new.clone()).unwrap();
        assert_eq!(c.current, id);
        assert_eq!(c.song(id.unwrap()).unwrap().station.name, "renamed");
        new.stations.pop();
        c.reload(new).unwrap();
        assert_eq!(c.current, None);
        assert_eq!(c.state, "stop");
    }
    #[test]
    fn metadata_changes_invalidate_queue_cache_but_duplicates_do_not() {
        let mut c = fixture();
        let queue = c.queue.clone();
        let current = c.current;
        let version = c.playlist_version;
        c.receive_metadata(Some("First title".into()), Some("Artist".into()));
        assert_eq!(c.playlist_version, version + 1);
        let revision = c.revision;
        c.receive_metadata(Some("First title".into()), Some("Artist".into()));
        c.receive_metadata(None, None);
        assert_eq!(c.playlist_version, version + 1);
        assert_eq!(c.revision, revision);
        c.receive_metadata(Some("Second title".into()), None);
        assert_eq!(c.artist.as_deref(), Some("Artist"));
        assert_eq!(c.playlist_version, version + 2);
        c.receive_metadata(None, Some("Another artist".into()));
        assert_eq!(c.title.as_deref(), Some("Second title"));
        assert_eq!(c.playlist_version, version + 3);
        assert_eq!(c.queue, queue);
        assert_eq!(c.current, current);
    }
    #[test]
    fn empty_metadata_clears_fields_and_invalidates_once() {
        let mut c = fixture();
        c.receive_metadata(Some("Title".into()), Some("Artist".into()));
        let version = c.playlist_version;
        c.receive_metadata(Some(String::new()), None);
        assert!(c.title.is_none());
        assert_eq!(c.artist.as_deref(), Some("Artist"));
        assert_eq!(c.playlist_version, version + 1);
        c.receive_metadata(None, Some(String::new()));
        assert!(c.artist.is_none());
        assert_eq!(c.playlist_version, version + 2);
        c.receive_metadata(Some(String::new()), Some(String::new()));
        assert_eq!(c.playlist_version, version + 2);
    }
    #[test]
    fn switching_station_invalidates_the_former_title_without_changing_ids() {
        let mut c = fixture();
        let queue = c.queue.clone();
        c.receive_metadata(Some("Old source title".into()), Some("Old artist".into()));
        let version = c.playlist_version;
        c.select(queue[1]).unwrap();
        assert_eq!(c.playlist_version, version + 1);
        assert_eq!(c.queue, queue);
        assert_eq!(c.current, Some(queue[1]));
        assert!(c.title.is_none() && c.artist.is_none());
    }
    #[test]
    fn reconnect_reset_invalidates_tags_but_not_queue_structure() {
        let mut c = fixture();
        let queue = c.queue.clone();
        let current = c.current;
        c.receive_metadata(Some("Old title".into()), None);
        let version = c.playlist_version;
        c.play().unwrap();
        assert_eq!(c.playlist_version, version + 1);
        assert!(c.title.is_none());
        c.play().unwrap();
        assert_eq!(c.playlist_version, version + 1);
        assert_eq!(c.queue, queue);
        assert_eq!(c.current, current);
    }
    #[test]
    fn volume_is_clamped() {
        let mut c = fixture();
        c.set_volume(100).unwrap();
        assert_eq!(c.volume, 70);
    }
    #[test]
    fn persistence_failure_does_not_become_stream_failure() {
        let mut c = fixture();
        c.path = PathBuf::from("/dev/null/state.json");
        c.play().unwrap();
        assert!(c.desired);
        assert!(c.persistence_error.is_some());
        assert_eq!(c.state, "buffering");
        assert!(c.retry.is_none());
        let mut new = c.config.clone();
        new.stations.reverse();
        c.reload(new).unwrap();
        assert_eq!(c.songs[0].station.name, "B");
        assert_eq!(c.config.stations[0].name, "B");
    }
    #[test]
    fn buffering_stall_times_out_without_resetting_backoff() {
        let mut c = fixture();
        c.desired = true;
        c.state = "play".into();
        c.stable = Some(Instant::now() - Duration::from_secs(60));
        c.attempt = 4;
        c.buffering(10);
        assert!(c.stable.is_none());
        c.buffering(100);
        assert_eq!(c.state, "buffering");
        c.started = Some(Instant::now() - Duration::from_secs(60));
        c.tick();
        assert_eq!(c.state, "reconnecting");
        assert_eq!(c.attempt, 5);
    }
    #[test]
    fn hardware_change_rejected_atomically() {
        let mut c = fixture();
        let mut new = c.config.clone();
        new.mixer_control = "missing".into();
        new.stations.clear();
        assert!(c.reload(new).is_err());
        assert_eq!(c.songs.len(), 2);
    }
    fn takeover(c: &mut Core) -> u64 {
        c.prepare_external(
            "owner",
            "http://127.0.0.1:1/media",
            Some("Track".into()),
            Some("Artist".into()),
        )
        .unwrap()
    }
    #[test]
    fn external_return_and_persistence() {
        let mut c = fixture();
        c.stop(false).unwrap();
        let radio = c.current;
        c.play().unwrap();
        takeover(&mut c);
        let ghost = c.current.unwrap();
        assert!(c.song(ghost).unwrap().temporary);
        assert_eq!(c.radio_song_ids().len(), 2);
        assert_eq!(c.title.as_deref(), Some("Track"));
        let saved: Saved = serde_json::from_slice(&std::fs::read(&c.path).unwrap()).unwrap();
        assert_eq!(
            saved.url,
            Some(c.song(radio.unwrap()).unwrap().station.url.clone())
        );
        assert!(saved.playing);
        c.external_play("owner").unwrap();
        assert_eq!(c.title.as_deref(), Some("Track"));
        c.external_stop("owner").unwrap();
        assert_eq!(c.current, radio);
        assert!(c.desired);
        assert!(c.song(ghost).is_none());
    }
    #[test]
    fn external_paused_stopped_and_global_stop() {
        for pause in [false, true] {
            let mut c = fixture();
            c.stop(pause).unwrap();
            let id = c.current;
            takeover(&mut c);
            c.external_stop("owner").unwrap();
            assert_eq!(c.current, id);
            assert!(!c.desired);
            assert_eq!(c.state, if pause { "pause" } else { "stop" });
        }
        let mut c = fixture();
        c.play().unwrap();
        let radio = c.current;
        takeover(&mut c);
        c.stop(false).unwrap();
        c.external_stop("owner").unwrap();
        assert_eq!(c.current, radio);
        assert!(!c.desired);
    }
    #[test]
    fn external_ownership_idempotence_and_selection() {
        let mut c = fixture();
        c.stop(false).unwrap();
        let radio = c.current;
        let session = takeover(&mut c);
        c.external_play("owner").unwrap();
        let ghost = c.current;
        assert_eq!(takeover(&mut c), session);
        assert_eq!(c.current, ghost);
        assert!(c.desired);
        assert!(
            c.prepare_external("other", "http://127.0.0.1:1/x", None, None)
                .is_err()
        );
        assert!(c.external_play("other").is_err());
        assert!(c.external_pause("other").is_err());
        assert!(c.external_stop("other").is_err());
        let next = c
            .prepare_external("owner", "http://127.0.0.1:1/next", None, None)
            .unwrap();
        assert!(next > session);
        c.external_stop("owner").unwrap();
        assert_eq!(c.current, radio);
        takeover(&mut c);
        let target = c.radio_song_ids()[1];
        c.select(target).unwrap();
        assert!(c.external_session_id().is_none());
        assert_eq!(c.current, Some(target));
        assert!(c.desired);
    }
    #[test]
    fn external_deadlines_and_failure_clock() {
        let mut c = fixture();
        c.stop(false).unwrap();
        let radio = c.current;
        takeover(&mut c);
        c.external.as_mut().unwrap().prepared = Some(Instant::now() - Duration::from_secs(100));
        c.tick();
        assert!(c.external.is_none());
        assert_eq!(c.current, radio);
        takeover(&mut c);
        c.external_play("owner").unwrap();
        c.failed("offline".into());
        let first = c.external.as_ref().unwrap().failed;
        c.start_source().unwrap();
        c.failed("offline again".into());
        assert_eq!(c.external.as_ref().unwrap().failed, first);
        c.external.as_mut().unwrap().failed = Some(Instant::now() - Duration::from_secs(100));
        c.tick();
        assert!(c.external.is_none());
        takeover(&mut c);
        c.external_play("owner").unwrap();
        c.failed("offline".into());
        let failed = c.external.as_ref().unwrap().failed;
        c.external_play("owner").unwrap();
        assert_eq!(c.external.as_ref().unwrap().failed, failed);
        c.external_pause("owner").unwrap();
        c.external_play("owner").unwrap();
        assert!(c.external.as_ref().unwrap().failed.is_none());
        c.external_pause("owner").unwrap();
        assert!(!c.desired);
        assert_eq!(c.state, "pause");
        assert!(c.retry.is_none());
    }
    #[test]
    fn external_reload_metadata_ids_and_mute() {
        let mut c = fixture();
        c.stop(false).unwrap();
        let ids = c.radio_song_ids();
        let version = c.playlist_version;
        takeover(&mut c);
        assert!(c.playlist_version > version);
        let ghost = c.current;
        c.set_mute(true).unwrap();
        c.upnp_set_volume(255).unwrap();
        assert!(c.muted);
        assert_eq!(c.effective_volume(), 0);
        assert_eq!(c.volume, c.config.max_volume);
        c.set_volume(255).unwrap();
        assert!(!c.muted);
        assert_eq!(c.effective_volume(), c.config.max_volume);
        let mut new = c.config.clone();
        new.stations.reverse();
        new.stations[0].name = "Renamed".into();
        c.reload(new).unwrap();
        assert_eq!(c.current, ghost);
        let mut reversed = ids.clone();
        reversed.reverse();
        assert_eq!(c.radio_song_ids(), reversed);
        let mut new = c.config.clone();
        new.stations
            .retain(|s| s.url != c.song(ids[0]).unwrap().station.url);
        c.reload(new).unwrap();
        c.external_stop("owner").unwrap();
        assert_eq!(c.current, None);
        assert!(!c.desired);
    }
    #[test]
    fn repeated_upnp_play_preserves_active_source_and_failure_grace() {
        let mut c = fixture();
        c.prepare_external(
            "127.0.0.1",
            "http://127.0.0.1:1/capture",
            Some("Track".into()),
            None,
        )
        .unwrap();
        c.external_play("127.0.0.1").unwrap();
        let session = c.external_session_id();
        let version = c.playlist_version;
        let revision = c.revision;
        let failed = Instant::now() - Duration::from_secs(2);
        c.external.as_mut().unwrap().failed = Some(failed);
        c.external_play("127.0.0.1").unwrap();
        assert_eq!(c.external_session_id(), session);
        assert_eq!(c.external.as_ref().unwrap().failed, Some(failed));
        assert_eq!(c.playlist_version, version);
        assert_eq!(c.revision, revision);
    }
    #[test]
    fn confirmed_external_recovery_cancels_stale_failure_deadline() {
        let mut c = fixture();
        takeover(&mut c);
        c.external_play("owner").unwrap();
        c.failed("transient failure".into());
        c.external.as_mut().unwrap().failed = Some(Instant::now() - Duration::from_secs(100));
        let session = c.external_session_id();
        c.confirmed_playing();
        assert!(c.external.as_ref().unwrap().failed.is_none());
        assert_eq!(c.external_session_id(), session);
        assert_eq!(c.state, "play");
        assert!(c.desired);
    }
    #[test]
    fn external_eos_returns_instead_of_retrying() {
        let mut c = fixture();
        c.stop(false).unwrap();
        let radio = c.current;
        c.play().unwrap();
        takeover(&mut c);
        c.external_play("owner").unwrap();
        c.ended();
        assert!(c.external.is_none());
        assert_eq!(c.current, radio);
        assert!(c.desired);
        assert!(c.retry.is_none());
    }
    #[test]
    fn deleted_return_is_neither_saved_nor_resumed() {
        let mut c = fixture();
        c.stop(false).unwrap();
        let radio = c.current.unwrap();
        c.play().unwrap();
        takeover(&mut c);
        c.external_play("owner").unwrap();
        c.queue.retain(|id| *id != radio);
        c.queue_updated();
        let saved: Saved = serde_json::from_slice(&std::fs::read(&c.path).unwrap()).unwrap();
        assert!(saved.url.is_none());
        assert!(!saved.playing);
        c.external_stop("owner").unwrap();
        assert!(c.current.is_none());
        assert!(!c.desired);
        assert_eq!(c.state, "stop");
        assert!(c.retry.is_none());
        assert!(c.external_session_id().is_none());
        assert_eq!(c.radio_song_ids().len(), 2);
        assert!(!c.queue.is_empty());
    }
    #[test]
    fn local_stop_blocks_metadata_recast_until_owner_stop() {
        let mut c = fixture();
        c.stop(false).unwrap();
        takeover(&mut c);
        c.external_play("owner").unwrap();
        c.stop(false).unwrap();
        let current = c.current;
        let revision = c.revision;
        assert!(
            c.prepare_external(
                "owner",
                "http://127.0.0.1:1/media",
                Some("New title".into()),
                None
            )
            .is_err()
        );
        assert_eq!(c.current, current);
        assert_eq!(c.revision, revision);
        assert_eq!(c.state, "stop");
        assert!(!c.desired);
        c.external_stop("unrelated").unwrap();
        assert!(
            c.prepare_external("owner", "http://127.0.0.1:1/media", None, None)
                .is_err()
        );
        c.save().unwrap();
        let bytes = std::fs::read(&c.path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("blocked_external_owner").is_none());
        assert!(!String::from_utf8(bytes).unwrap().contains("owner"));
        let restarted = Core::new(
            c.config.clone(),
            Audio::new("default", "default", "Master", true).unwrap(),
            c.path.clone(),
        )
        .unwrap();
        assert!(restarted.blocked_external_owner.is_none());
        c.external_stop("owner").unwrap();
        takeover(&mut c);
        assert_eq!(c.external_owner(), Some("owner"));
    }
    #[test]
    fn radio_selection_blocks_old_owner_but_not_new_owner() {
        let mut c = fixture();
        c.stop(false).unwrap();
        takeover(&mut c);
        let target = c.radio_song_ids()[1];
        c.select(target).unwrap();
        assert!(
            c.prepare_external("owner", "http://127.0.0.1:1/media", None, None)
                .is_err()
        );
        assert_eq!(c.current, Some(target));
        assert!(c.desired);
        c.prepare_external("other", "http://127.0.0.1:1/other", None, None)
            .unwrap();
        let other = c.external_session_id();
        assert!(c.external_stop("owner").is_err());
        assert_eq!(c.external_session_id(), other);
        assert_eq!(c.blocked_external_owner.as_deref(), Some("owner"));
        c.external_stop("other").unwrap();
        assert_eq!(c.current, Some(target));
        assert!(c.desired);
        c.external_stop("owner").unwrap();
        takeover(&mut c);
    }
}
