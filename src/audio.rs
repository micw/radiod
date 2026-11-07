use alsa::mixer::{Mixer, SelemId};
use anyhow::{Context, Result, anyhow, bail};
use gstreamer::{self as gst, prelude::*};

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
    active: bool,
    live: bool,
    buffering: bool,
}

impl Audio {
    pub fn new(
        device: &str,
        mixer_device: &str,
        mixer_control: &str,
        null_output: bool,
    ) -> Result<Self> {
        gst::init().context("GStreamer initialization failed")?;
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
        pipeline.set_property_from_str("flags", "audio");
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
        let bus = pipeline.bus().context("playbin has no bus")?;
        Ok(Self {
            pipeline,
            bus,
            mixer,
            active: false,
            live: false,
            buffering: false,
        })
    }

    pub fn play(&mut self, url: &str) -> Result<()> {
        self.stop()?;
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
        // Flush before teardown as well as afterwards, so messages produced by
        // the old stream cannot masquerade as events from a subsequent play.
        self.bus.set_flushing(true);
        let result = self.pipeline.set_state(gst::State::Null);
        if result.is_ok() {
            let (settled, _, _) = self.pipeline.state(gst::ClockTime::from_seconds(3));
            self.bus.set_flushing(false);
            settled.context("Playback shutdown did not settle")?;
        } else {
            self.bus.set_flushing(false);
        }
        while self.bus.pop().is_some() {}
        result.context("Cannot stop playback")?;
        Ok(())
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
            let value = min + ((max - min) as f64 * f64::from(volume) / 100.0).round() as i64;
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
                        "GStreamer error: resource={:?}, stream={:?}, core={:?}",
                        e.kind::<gst::ResourceError>(),
                        e.kind::<gst::StreamError>(),
                        e.kind::<gst::CoreError>()
                    )));
                    let _ = self.stop();
                    break;
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
                    let mut latency = gst::query::Latency::new();
                    if self.pipeline.query(&mut latency) && latency.result().0 {
                        self.live = true;
                    }
                    // Live-ness can only become known after preroll. If a prior
                    // buffering message paused the pipeline, explicitly undo it.
                    if self.live {
                        let was_paused = std::mem::replace(&mut self.buffering, false);
                        if was_paused
                            && let Err(error) = self.pipeline.set_state(gst::State::Playing)
                        {
                            events.push(Event::Error(error.to_string()));
                            let _ = self.stop();
                            break;
                        }
                    } else {
                        events.push(Event::Buffering(percent));
                        let pause = percent < 100;
                        if self.buffering != pause {
                            self.buffering = pause;
                            let state = if pause {
                                gst::State::Paused
                            } else {
                                gst::State::Playing
                            };
                            match self.pipeline.set_state(state) {
                                Ok(gst::StateChangeSuccess::NoPreroll) => {
                                    self.live = true;
                                    self.buffering = false;
                                    if pause
                                        && let Err(error) =
                                            self.pipeline.set_state(gst::State::Playing)
                                    {
                                        events.push(Event::Error(error.to_string()));
                                        let _ = self.stop();
                                        break;
                                    }
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    events.push(Event::Error(error.to_string()));
                                    let _ = self.stop();
                                    break;
                                }
                            }
                        }
                    }
                    if (self.live || percent == 100)
                        && self.pipeline.current_state() == gst::State::Playing
                    {
                        events.push(Event::Playing);
                    }
                }
                MessageView::StateChanged(state)
                    if message
                        .src()
                        .is_some_and(|src| src == self.pipeline.upcast_ref::<gst::Object>())
                        && state.current() == gst::State::Playing
                        && !self.buffering =>
                {
                    events.push(Event::Playing);
                }
                _ => {}
            }
        }
        events
    }
}

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
    fn live_discovery_resumes_a_previously_buffer_paused_pipeline() {
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
