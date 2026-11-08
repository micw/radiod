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
}
#[derive(Serialize, Deserialize, Default)]
struct Saved {
    url: Option<String>,
    volume: Option<u8>,
    #[serde(default)]
    playing: bool,
}
pub struct Core {
    pub config: Config,
    pub songs: Vec<Song>,
    pub queue: Vec<u32>,
    pub current: Option<u32>,
    pub volume: u8,
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
        audio.set_volume(volume)?;
        let songs: Vec<_> = config
            .stations
            .iter()
            .enumerate()
            .map(|(i, s)| Song {
                id: i as u32 + 1,
                station: s.clone(),
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
        let saved = Saved {
            url: self
                .current
                .and_then(|id| self.song(id).map(|s| s.station.url.clone())),
            volume: Some(self.volume),
            playing: self.desired,
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
        if self.current != Some(id) {
            // Invalidate the former queue entry before changing its ownership.
            self.set_metadata(None, None);
        }
        self.current = Some(id);
        self.attempt = 0;
        self.play()
    }
    pub fn stop(&mut self, pause: bool) -> Result<()> {
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
        self.changed();
        self.persist();
        Ok(())
    }
    fn failed(&mut self, error: String) {
        let _ = self.audio.stop();
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
    pub fn tick(&mut self) {
        if self.desired {
            for e in self.audio.poll() {
                match e {
                    Event::Playing => {
                        if self.state != "play" {
                            eprintln!("audio state=playing station_id={:?}", self.current);
                        }
                        self.state = "play".into();
                        self.started = None;
                        self.stable.get_or_insert_with(Instant::now);
                        self.error = None;
                        self.changed();
                    }
                    Event::Buffering(p) => self.buffering(p),
                    Event::Tags { title, artist } => self.receive_metadata(title, artist),
                    Event::Ended => {
                        self.failed("unexpected end of live stream".into());
                        break;
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
            if self.stable.is_some_and(|t| t.elapsed().as_secs() >= 30) {
                self.attempt = 0;
            }
            if self.retry.is_some_and(|t| Instant::now() >= t)
                && let Err(e) = self.play()
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
                && new.mixer_control == self.config.mixer_control,
            "listen/audio device changes require restart"
        );
        // Build the complete replacement before touching live metadata.
        let old: HashMap<_, _> = self
            .songs
            .iter()
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
            });
        }
        let removed = self
            .current
            .is_some_and(|id| !songs.iter().any(|s| s.id == id));
        let volume = self.volume.min(new.max_volume);
        // Hardware failures reject the config before changing stations/queue.
        if volume != self.volume {
            self.audio.set_volume(volume)?;
        }
        if removed && let Err(e) = self.audio.stop() {
            let _ = self.audio.set_volume(self.volume);
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
        self.volume = volume;
        self.config = new;
        self.changed();
        // Persistence is reported separately; disk errors must not partly reject
        // an otherwise accepted config or interrupt a healthy stream.
        self.persist();
        Ok(())
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
}
