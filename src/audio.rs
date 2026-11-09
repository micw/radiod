use crate::volume::{normalized_db, raw_volume};
use alsa::{
    Round,
    mixer::{MilliBel, Mixer, SelemId},
};
use anyhow::{Context, Result, anyhow, bail};
use gstreamer::{self as gst, prelude::*};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn configure_output_queue(element: &gst::Element, external: bool) {
    if element.name() == "aqueue" && element.factory().is_some_and(|f| f.name() == "queue") {
        // playsink has its own one-second PCM queue, independent of playbin's
        // upstream buffer-duration. Leaving it unchanged hides another second
        // behind a paused HTTP refill. Bound it only for LAN capture.
        element.set_property(
            "max-size-time",
            if external {
                60_000_000u64
            } else {
                1_000_000_000u64
            },
        );
        element.set_property(
            "max-size-bytes",
            if external { 0u32 } else { 10_485_760u32 },
        );
        element.set_property("max-size-buffers", if external { 0u32 } else { 200u32 });
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Playing,
    Buffering(u8),
    // Missing fields mean “unchanged”, not “clear the previous value”.
    Tags {
        title: Option<String>,
        artist: Option<String>,
    },
    Ended,
    Error(String),
}

pub struct Audio {
    pipeline: gst::Element,
    bus: gst::Bus,
    mixer: Option<(Mixer, SelemId)>,
    external_profile: Arc<AtomicBool>,
    active: bool,
    live: bool,
    buffering: bool,
    playing_reported: bool,
    http_status: Option<u32>,
    trace: bool,
}

impl Audio {
    pub fn new(
        device: &str,
        mixer_device: &str,
        mixer_control: &str,
        null_output: bool,
    ) -> Result<Self> {
        gst::init().context("GStreamer initialization failed")?;
        eprintln!("audio engine: {}", gst::version_string());
        let pipeline = gst::ElementFactory::make("playbin")
            .build()
            .context("GStreamer playbin is unavailable")?;
        let sink = if null_output {
            gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()
                .context("GStreamer fakesink is unavailable")?
        } else {
            gst::ElementFactory::make("alsasink")
                .property("device", device)
                .build()
                .context("GStreamer alsasink is unavailable")?
        };
        pipeline.set_property("audio-sink", &sink);
        // Volume is exclusively controlled by the hardware mixer.
        pipeline.set_property("volume", 1.0f64);
        pipeline.set_property("mute", false);
        // Radio playback must not create video windows or subtitle outputs.
        // GstPlayFlags: AUDIO (1 << 1), SOFT_VOLUME intentionally disabled.
        pipeline.set_property_from_str("flags", "audio+buffering");
        // Parsed audio timestamps bound startup buffering independently of a
        // source's burst download rate. Keep a small, explicit radio buffer.
        pipeline.set_property("buffer-duration", 1_000_000_000i64);
        pipeline.set_property("buffer-size", 262_144i32);
        pipeline.connect("source-setup", false, |values| {
            if let Ok(source) = values[1].get::<gst::Element>() {
                // Units differ between the common HTTP sources. Only touch
                // properties whose type we know; other protocols keep defaults.
                let factory = source
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                if factory == "souphttpsrc" {
                    source.set_property("timeout", 15u32); // seconds
                    if source.find_property("retries").is_some() {
                        source.set_property("retries", 0i32); // retry policy belongs to Core
                    }
                } else if factory == "rtspsrc" {
                    source.set_property("timeout", 15_000_000u64); // microseconds
                    source.set_property("tcp-timeout", 15_000_000u64);
                }
            }
            None
        });
        let mixer = if null_output {
            None
        } else {
            let mixer = Mixer::new(mixer_device, false)
                .with_context(|| format!("Cannot open ALSA mixer {mixer_device}"))?;
            let id = SelemId::new(mixer_control, 0);
            {
                let element = mixer.find_selem(&id).ok_or_else(|| {
                    anyhow!("ALSA mixer control {mixer_control:?} not found on {mixer_device}")
                })?;
                if !element.has_playback_volume() {
                    bail!("ALSA mixer control {mixer_control:?} has no playback volume");
                }
            }
            Some((mixer, id))
        };
        let external_profile = Arc::new(AtomicBool::new(false));
        let profile = external_profile.clone();
        pipeline.connect("deep-element-added", false, move |values| {
            if let Ok(element) = values[2].get::<gst::Element>() {
                configure_output_queue(&element, profile.load(Ordering::Acquire));
            }
            None
        });
        let bus = pipeline.bus().context("playbin has no bus")?;
        Ok(Self {
            pipeline,
            bus,
            mixer,
            external_profile,
            active: false,
            live: false,
            buffering: false,
            playing_reported: false,
            http_status: None,
            trace: std::env::var_os("RADIOD_TRACE_AUDIO").is_some(),
        })
    }

    pub fn play(&mut self, url: &str) -> Result<()> {
        self.play_with_profile(url, false)
    }

    pub fn play_external(&mut self, url: &str) -> Result<()> {
        self.play_with_profile(url, true)
    }

    fn configure_profile(&mut self, external: bool) {
        self.external_profile.store(external, Ordering::Release);
        if let Some(bin) = self.pipeline.downcast_ref::<gst::Bin>() {
            let mut children = bin.iterate_recurse();
            while let Ok(Some(element)) = children.next() {
                configure_output_queue(&element, external);
            }
        }
        // LAN capture needs responsive output, not the Internet-radio refill
        // margin. Always restore the radio settings when leaving that input.
        self.pipeline.set_property(
            "buffer-duration",
            if external {
                200_000_000i64
            } else {
                1_000_000_000i64
            },
        );
        self.pipeline
            .set_property("buffer-size", if external { 32_768i32 } else { 262_144i32 });
        let sink = self.pipeline.property::<gst::Element>("audio-sink");
        if sink.find_property("buffer-time").is_some() {
            sink.set_property("buffer-time", if external { 60_000i64 } else { 200_000i64 });
            sink.set_property("latency-time", 10_000i64);
        }
    }

    fn play_with_profile(&mut self, url: &str, external: bool) -> Result<()> {
        self.stop()?;
        self.configure_profile(external);
        if url.trim().is_empty() {
            bail!("Playback URI must not be empty");
        }
        self.pipeline.set_property("uri", url);
        self.active = true;
        match self.pipeline.set_state(gst::State::Playing) {
            Ok(result) => {
                self.live = result == gst::StateChangeSuccess::NoPreroll;
                Ok(())
            }
            Err(error) => {
                let _ = self.stop();
                Err(error).context("Cannot start playback")
            }
        }
    }

    pub fn stop(&mut self) -> Result<()> {
        self.active = false;
        self.live = false;
        self.buffering = false;
        self.playing_reported = false;
        self.http_status = None;
        // Flush before teardown as well as afterwards, so messages produced by
        // the old stream cannot masquerade as events from a subsequent play.
        self.bus.set_flushing(true);
        // A failed upward transition can race the first downward request and
        // leave the parent at READY. Reassert NULL, never a new input, within the
        // original three-second shutdown budget. Only observed settled NULL is
        // success; neither an OK return nor a stale failure alone is authoritative.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut observed = (gst::State::VoidPending, gst::State::VoidPending);
        let mut stopped = false;
        for _ in 0..3 {
            let _ = self.pipeline.set_state(gst::State::Null);
            let wait = deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(std::time::Duration::from_secs(1));
            let (_, current, pending) = self
                .pipeline
                .state(gst::ClockTime::from_nseconds(wait.as_nanos() as u64));
            observed = (current, pending);
            if current == gst::State::Null && pending == gst::State::VoidPending {
                stopped = true;
                break;
            }
            std::thread::yield_now();
        }
        self.bus.set_flushing(false);
        while self.bus.pop().is_some() {}
        if stopped {
            Ok(())
        } else {
            bail!(
                "Cannot stop playback: current={:?}, pending={:?}",
                observed.0,
                observed.1
            )
        }
    }

    pub fn set_volume(&mut self, volume: u8) -> Result<()> {
        if volume > 100 {
            bail!("Volume must be between 0 and 100");
        }
        if let Some((mixer, id)) = &self.mixer {
            let element = mixer
                .find_selem(id)
                .context("ALSA mixer control disappeared")?;
            let (min, max) = element.get_playback_volume_range();
            let fallback =
                raw_volume(volume, min, max).context("Invalid ALSA playback volume range")?;
            // Endpoints must be exact, including mute controls. Querying endpoint
            // conversions checks ALSA's Result status, unlike the crate's dB
            // range getter, which discards errors and can fabricate (0, 0).
            // All conversions are read-only; complete them before any setter.
            let value = if volume == 0 || volume == 100 || min == max {
                fallback
            } else {
                let metadata = element.ask_playback_vol_db(min).and_then(|db_min| {
                    element
                        .ask_playback_vol_db(max)
                        .map(|db_max| (db_min.0, db_max.0))
                });
                match metadata
                    .ok()
                    .and_then(|(lo, hi)| normalized_db(volume, lo, hi))
                {
                    Some(db) => element
                        // MPD supports directed rounding. Choose ceil both for
                        // centi-dB and the supported hardware step (not raw
                        // interpolation); clamp even a surprising ALSA result.
                        .ask_playback_db_vol(MilliBel(db), Round::Ceil)
                        .context("Cannot convert ALSA playback dB to hardware volume")?
                        .clamp(min, max),
                    None => fallback,
                }
            };
            element
                .set_playback_volume_all(value)
                .context("Cannot set ALSA playback volume")?;
            if element.has_playback_switch() {
                element
                    .set_playback_switch_all(i32::from(volume != 0))
                    .context("Cannot set ALSA playback switch")?;
            }
        }
        Ok(())
    }

    fn reconcile_state(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let target = if self.live || !self.buffering {
            gst::State::Playing
        } else {
            gst::State::Paused
        };
        let (_, current, pending) = self.pipeline.state(gst::ClockTime::ZERO);
        let settled_wrong = pending == gst::State::VoidPending && current != target;
        let pending_wrong = pending != gst::State::VoidPending && pending != target;
        if settled_wrong || pending_wrong {
            if self.trace {
                eprintln!(
                    "gst reconcile target={target:?} current={current:?} pending={pending:?}"
                );
            }
            let result = self
                .pipeline
                .set_state(target)
                .context("GStreamer state transition failed")?;
            if result == gst::StateChangeSuccess::NoPreroll {
                self.live = true;
                self.buffering = false;
                if target != gst::State::Playing {
                    self.pipeline
                        .set_state(gst::State::Playing)
                        .context("resume live source")?;
                }
            }
        }
        Ok(())
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(message) = self.bus.pop() {
            if !self.active {
                continue;
            }
            use gst::MessageView;
            match message.view() {
                MessageView::Error(error) => {
                    // Upstream messages/debug strings may contain credentialed URLs.
                    // Expose typed diagnostics, never the unfiltered source text.
                    let e = error.error();
                    events.push(Event::Error(format!(
                        "GStreamer error: resource={:?}, stream={:?}, core={:?}, http_status={:?}",
                        e.kind::<gst::ResourceError>(),
                        e.kind::<gst::StreamError>(),
                        e.kind::<gst::CoreError>(),
                        self.http_status
                    )));
                    let _ = self.stop();
                    break;
                }
                MessageView::Element(element) => {
                    if let Some(s) = element.structure().filter(|s| s.name() == "http-headers") {
                        self.http_status = s.get::<u32>("http-status-code").ok();
                    }
                }
                MessageView::Eos(..) => {
                    events.push(Event::Ended);
                    let _ = self.stop();
                    break;
                }
                MessageView::Tag(tags) => {
                    let tags = tags.tags();
                    let title = tags.get::<gst::tags::Title>().map(|v| v.get().to_owned());
                    let artist = tags.get::<gst::tags::Artist>().map(|v| v.get().to_owned());
                    if title.is_some() || artist.is_some() {
                        events.push(Event::Tags { title, artist });
                    }
                }
                MessageView::Buffering(buffering) => {
                    let percent = buffering.percent().clamp(0, 100) as u8;
                    if self.trace {
                        let (_, current, pending) = self.pipeline.state(gst::ClockTime::ZERO);
                        eprintln!(
                            "gst buffering={percent} live={} buffer_paused={} current={current:?} pending={pending:?}",
                            self.live, self.buffering
                        );
                    }
                    // Coalesce a bus batch into the latest desired state.
                    // Historical 0/100% messages must not churn asynchronous
                    // pause/resume requests that can leave an older pause pending.
                    if self.live {
                        self.buffering = false;
                    } else {
                        self.buffering = percent < 100;
                        if self.buffering {
                            self.playing_reported = false;
                        }
                        events.push(Event::Buffering(percent));
                    }
                }
                MessageView::StateChanged(state)
                    if self.trace
                        && message
                            .src()
                            .is_some_and(|s| s == self.pipeline.upcast_ref::<gst::Object>()) =>
                {
                    eprintln!(
                        "gst pipeline_transition old={:?} new={:?} pending={:?}",
                        state.old(),
                        state.current(),
                        state.pending()
                    );
                }
                MessageView::AsyncDone(..) if self.trace => {
                    let (_, current, pending) = self.pipeline.state(gst::ClockTime::ZERO);
                    eprintln!("gst async_done current={current:?} pending={pending:?}");
                }
                MessageView::Latency(..) => {
                    if let Some(bin) = self.pipeline.downcast_ref::<gst::Bin>() {
                        let _ = bin.recalculate_latency();
                    }
                }
                _ => {}
            }
        }
        if let Err(error) = self.reconcile_state() {
            events.push(Event::Error(error.to_string()));
            let _ = self.stop();
        }
        // Bus messages describe past transitions. Confirm current AND pending
        // state after processing the batch before reporting output as playing.
        let (_, current, pending) = self.pipeline.state(gst::ClockTime::ZERO);
        let playing = self.active
            && !self.buffering
            && current == gst::State::Playing
            && pending == gst::State::VoidPending;
        if playing && !self.playing_reported {
            events.push(Event::Playing);
        }
        self.playing_reported = playing;
        events
    }
}

#[cfg(test)]
#[path = "audio_latency_tests.rs"]
mod latency_tests;

impl Drop for Audio {
    fn drop(&mut self) {
        self.bus.set_flushing(true);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nonlive_buffering_does_not_use_sink_latency_as_source_liveness() {
        gst::init().unwrap();
        let mut audio = Audio::new("default", "default", "Master", true).unwrap();
        audio.pipeline =
            gst::parse::launch("audiotestsrc is-live=false ! fakesink sync=true").unwrap();
        audio.bus = audio.pipeline.bus().unwrap();
        let start = audio.pipeline.set_state(gst::State::Playing).unwrap();
        assert_ne!(start, gst::StateChangeSuccess::NoPreroll);
        let (result, current, _) = audio.pipeline.state(gst::ClockTime::from_seconds(2));
        result.unwrap();
        assert_eq!(current, gst::State::Playing);
        while audio.bus.pop().is_some() {}
        audio.active = true;
        audio
            .bus
            .post(
                gst::message::Buffering::builder(10)
                    .src(&audio.pipeline)
                    .build(),
            )
            .unwrap();
        let events = audio.poll();
        assert!(
            !audio.live,
            "synchronous sink must not classify a non-live source as live"
        );
        assert!(audio.buffering);
        assert!(!events.iter().any(|e| matches!(e, Event::Playing)));
        let (_, current, _) = audio.pipeline.state(gst::ClockTime::from_seconds(2));
        assert_eq!(current, gst::State::Paused);
        audio
            .bus
            .post(
                gst::message::Buffering::builder(100)
                    .src(&audio.pipeline)
                    .build(),
            )
            .unwrap();
        audio.poll();
        let (_, current, _) = audio.pipeline.state(gst::ClockTime::from_seconds(2));
        assert_eq!(current, gst::State::Playing);
        assert!(!audio.buffering);
    }
    #[test]
    fn external_profile_bounds_new_and_reused_output_queue_then_restores_radio() {
        let mut audio = Audio::new("default", "default", "Master", true).unwrap();
        audio.configure_profile(true);
        let queue = gst::ElementFactory::make("queue")
            .name("aqueue")
            .build()
            .unwrap();
        audio
            .pipeline
            .downcast_ref::<gst::Bin>()
            .unwrap()
            .add(&queue)
            .unwrap();
        assert_eq!(queue.property::<u64>("max-size-time"), 60_000_000);
        assert_eq!(
            audio.pipeline.property::<i64>("buffer-duration"),
            200_000_000
        );
        assert_eq!(audio.pipeline.property::<i32>("buffer-size"), 32_768);
        audio.configure_profile(false);
        assert_eq!(queue.property::<u64>("max-size-time"), 1_000_000_000);
        assert_eq!(queue.property::<u32>("max-size-bytes"), 10_485_760);
        assert_eq!(queue.property::<u32>("max-size-buffers"), 200);
        assert_eq!(
            audio.pipeline.property::<i64>("buffer-duration"),
            1_000_000_000
        );
        assert_eq!(audio.pipeline.property::<i32>("buffer-size"), 262_144);
        audio.configure_profile(true);
        assert_eq!(queue.property::<u64>("max-size-time"), 60_000_000);
    }
    #[test]
    fn failed_http_start_and_teardown_never_reuse_an_unsettled_pipeline() {
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let mut audio = Audio::new("default", "default", "Master", true).unwrap();
                    for _ in 0..64 {
                        let _ = audio.play("http://127.0.0.1:1/refused");
                        std::thread::yield_now();
                        audio.stop().unwrap();
                        let (_, current, pending) = audio.pipeline.state(gst::ClockTime::ZERO);
                        assert_eq!(current, gst::State::Null);
                        assert_eq!(pending, gst::State::VoidPending);
                    }
                });
            }
        });
    }
    #[test]
    fn full_buffer_reasserts_playing_after_an_asynchronous_pause() {
        gst::init().unwrap();
        let mut audio = Audio::new("default", "default", "Master", true).unwrap();
        audio.pipeline =
            gst::parse::launch("audiotestsrc is-live=false ! fakesink sync=true").unwrap();
        audio.bus = audio.pipeline.bus().unwrap();
        audio.pipeline.set_state(gst::State::Paused).unwrap();
        let (_, state, _) = audio.pipeline.state(gst::ClockTime::from_seconds(2));
        assert_eq!(state, gst::State::Paused);
        while audio.bus.pop().is_some() {}
        audio.active = true;
        audio.buffering = false; // latest BUFFERING(100) was already consumed
        audio.poll();
        let (_, state, pending) = audio.pipeline.state(gst::ClockTime::from_seconds(2));
        assert_eq!(state, gst::State::Playing);
        assert_eq!(pending, gst::State::VoidPending);
        audio.stop().unwrap();
        audio.poll();
        assert_eq!(audio.pipeline.current_state(), gst::State::Null);
    }
    #[test]
    fn historical_playing_message_cannot_override_actual_paused_state() {
        let mut audio = Audio::new("default", "default", "Master", true).unwrap();
        audio.active = true;
        audio
            .bus
            .post(
                gst::message::StateChanged::builder(
                    gst::State::Paused,
                    gst::State::Playing,
                    gst::State::VoidPending,
                )
                .src(&audio.pipeline)
                .build(),
            )
            .unwrap();
        assert!(!audio.poll().iter().any(|e| matches!(e, Event::Playing)));
    }
    #[test]
    fn known_live_source_does_not_remain_buffer_paused() {
        gst::init().unwrap();
        let mut audio = Audio::new("default", "default", "Master", true).unwrap();
        audio.pipeline =
            gst::parse::launch("audiotestsrc is-live=true ! fakesink sync=true").unwrap();
        audio.bus = audio.pipeline.bus().unwrap();
        audio.pipeline.set_state(gst::State::Paused).unwrap();
        audio.active = true;
        audio.live = true; // discovered after the earlier buffering pause
        audio.buffering = true;
        audio
            .bus
            .post(
                gst::message::Buffering::builder(10)
                    .src(&audio.pipeline)
                    .build(),
            )
            .unwrap();
        audio.poll();
        assert!(
            !audio.buffering,
            "live discovery must release the old pause"
        );
        let (result, state, _) = audio.pipeline.state(gst::ClockTime::from_seconds(2));
        result.unwrap();
        assert_eq!(state, gst::State::Playing);
    }
}
