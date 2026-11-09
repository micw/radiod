//! Generation-local, bounded RTP receiver. No network input is logged.
use crate::audio::Event;
use anyhow::{Context, Result, bail};
use gstreamer::{self as gst, prelude::*};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpFormat {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub rtp: RtpParameters,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpParameters {
    pub encoding_name: String,
    pub payload_type: u8,
    pub ssrc: u32,
    pub clock_rate: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fmtp: Option<RtpFmtp>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpFmtp {
    pub mode: String,
    pub config: String,
    pub size_length: u8,
    pub index_length: u8,
    pub index_delta_length: u8,
}
impl RtpFormat {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.sample_rate, 44100 | 48000)
            || self.rtp.clock_rate != self.sample_rate
            || !(96..=127).contains(&self.rtp.payload_type)
            || self.rtp.ssrc == 0
        {
            bail!("Invalid RTP rate, payload type or SSRC");
        }
        match self.codec.as_str() {
            "sbc"
                if self.rtp.encoding_name == "SBC"
                    && matches!(self.channels, 1 | 2)
                    && self.rtp.fmtp.is_none() => {}
            "aac-lc" if self.rtp.encoding_name == "MPEG4-GENERIC" && self.channels == 2 => {
                let f = self.rtp.fmtp.as_ref().context("AAC requires fmtp")?;
                if f.mode != "AAC-hbr"
                    || f.config
                        != if self.sample_rate == 44100 {
                            "1210"
                        } else {
                            "1190"
                        }
                    || (f.size_length, f.index_length, f.index_delta_length) != (13, 3, 3)
                {
                    bail!("Unsupported AAC fmtp");
                }
            }
            _ => bail!("Unsupported RTP audio format"),
        }
        Ok(())
    }
    fn caps(&self) -> gst::Caps {
        let mut b = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .field("encoding-name", &self.rtp.encoding_name)
            .field("clock-rate", self.sample_rate as i32)
            .field("payload", self.rtp.payload_type as i32)
            .field("ssrc", self.rtp.ssrc);
        if let Some(f) = &self.rtp.fmtp {
            b = b
                .field("mode", &f.mode)
                .field("config", &f.config)
                .field("sizelength", "13")
                .field("indexlength", "3")
                .field("indexdeltalength", "3")
                .field("encoding-params", "2")
                .field("streamtype", "5");
        }
        b.build()
    }
}

fn valid_packet(p: &[u8], format: &RtpFormat) -> bool {
    if p.len() < 12
        || p[0] >> 6 != 2
        || p[1] & 127 != format.rtp.payload_type
        || u32::from_be_bytes(p[8..12].try_into().unwrap()) != format.rtp.ssrc
    {
        return false;
    }
    let mut start = 12 + usize::from(p[0] & 15) * 4;
    if start > p.len() {
        return false;
    }
    if p[0] & 16 != 0 {
        if start + 4 > p.len() {
            return false;
        }
        let words = u16::from_be_bytes([p[start + 2], p[start + 3]]) as usize;
        start += 4 + words * 4;
        if start > p.len() {
            return false;
        }
    }
    let padding = if p[0] & 32 != 0 {
        usize::from(*p.last().unwrap())
    } else {
        0
    };
    if p[0] & 32 != 0 && padding == 0 {
        return false;
    }
    p.len()
        .checked_sub(start + padding)
        .is_some_and(|len| len > 0 && len <= 1500)
}
#[derive(Default)]
struct Counters {
    accepted: AtomicU64,
    last_sequence: AtomicU64,
    rejected: AtomicU64,
    decoded: AtomicU64,
    output: AtomicU64,
    socket_errors: AtomicU64,
    push_errors: AtomicU64,
    last_packet: Mutex<Option<Instant>>,
    last_output: Mutex<Option<Instant>>,
}
pub struct RtpPlayback {
    pipeline: gst::Pipeline,
    jitter: gst::Element,
    bus: gst::Bus,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
    started: Instant,
    buffer_ms: u32,
    reported: bool,
    terminal: bool,
}
impl RtpPlayback {
    pub fn start(
        format: &RtpFormat,
        listen: SocketAddr,
        peer: IpAddr,
        buffer_ms: u32,
        device: &str,
        null: bool,
    ) -> Result<(Self, u16)> {
        format.validate()?;
        if !(50..=5000).contains(&buffer_ms) || peer.is_unspecified() || peer.is_multicast() {
            bail!("Invalid RTP receiver settings");
        }
        let socket = UdpSocket::bind(listen).context("Cannot bind RTP socket")?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let port = socket.local_addr()?.port();
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("appsrc")
            .property("is-live", true)
            .property("format", gst::Format::Time)
            .property("do-timestamp", true)
            .property("block", false)
            .property("max-bytes", 262_144u64)
            .property_from_str("leaky-type", "downstream")
            .property("caps", format.caps())
            .build()?;
        let jitter = gst::ElementFactory::make("rtpjitterbuffer")
            .property("latency", buffer_ms)
            .property("drop-on-latency", true)
            .property("do-lost", true)
            .build()?;
        let aac = format.codec == "aac-lc";
        let depay =
            gst::ElementFactory::make(if aac { "rtpmp4gdepay" } else { "rtpsbcdepay" }).build()?;
        let parse = gst::ElementFactory::make(if aac { "aacparse" } else { "sbcparse" }).build()?;
        let decode = gst::ElementFactory::make(if aac { "avdec_aac" } else { "sbcdec" }).build()?;
        let queue = gst::ElementFactory::make("queue")
            .property("max-size-time", 60_000_000u64)
            .property("max-size-bytes", 0u32)
            .property("max-size-buffers", 0u32)
            .build()?;
        let convert = gst::ElementFactory::make("audioconvert").build()?;
        let sink = if null {
            gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()?
        } else {
            gst::ElementFactory::make("alsasink")
                .property("device", device)
                .property("buffer-time", 60_000i64)
                .property("latency-time", 10_000i64)
                .build()?
        };
        let elements = [
            &source, &jitter, &depay, &parse, &decode, &queue, &convert, &sink,
        ];
        pipeline.add_many(elements)?;
        gst::Element::link_many(elements)?;
        let counters = Arc::new(Counters::default());
        let c = counters.clone();
        decode
            .static_pad("src")
            .context("Decoder has no output pad")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                c.decoded.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        let c = counters.clone();
        sink.static_pad("sink")
            .context("Sink has no input pad")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                c.output.fetch_add(1, Ordering::Relaxed);
                *c.last_output.lock().unwrap() = Some(Instant::now());
                gst::PadProbeReturn::Ok
            });
        let bus = pipeline.bus().context("RTP pipeline has no bus")?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut playback = Self {
            pipeline,
            jitter,
            bus,
            stop: stop.clone(),
            worker: None,
            counters: counters.clone(),
            started: Instant::now(),
            buffer_ms,
            reported: false,
            terminal: false,
        };
        playback
            .pipeline
            .set_state(gst::State::Playing)
            .context("Cannot start RTP pipeline")?;
        let format = format.clone();
        playback.worker = Some(
            std::thread::Builder::new()
                .name("rtp-input".into())
                .spawn(move || {
                    // Full datagram buffer prevents truncation from converting oversized input to a valid packet.
                    let mut bytes = [0u8; 65536];
                    while !stop.load(Ordering::Acquire) {
                        match socket.recv_from(&mut bytes) {
                            Ok((n, sender)) => {
                                if stop.load(Ordering::Acquire) {
                                    break;
                                }
                                if sender.ip() != peer || !valid_packet(&bytes[..n], &format) {
                                    counters.rejected.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                                counters.last_sequence.store(
                                    u64::from(u16::from_be_bytes([bytes[2], bytes[3]])),
                                    Ordering::Relaxed,
                                );
                                counters.accepted.fetch_add(1, Ordering::Relaxed);
                                *counters.last_packet.lock().unwrap() = Some(Instant::now());
                                let buffer = gst::Buffer::from_mut_slice(bytes[..n].to_vec());
                                let result = source
                                    .emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]);
                                if result != gst::FlowReturn::Ok {
                                    counters.push_errors.fetch_add(1, Ordering::Relaxed);
                                    break;
                                }
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::TimedOut
                                        | std::io::ErrorKind::Interrupted
                                ) => {}
                            Err(_) => {
                                counters.socket_errors.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                })
                .context("Cannot start RTP worker")?,
        );
        Ok((playback, port))
    }
    pub fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let worker_panicked = self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err());
        self.bus.set_flushing(true);
        let deadline = Instant::now() + Duration::from_secs(3);
        for _ in 0..3 {
            let _ = self.pipeline.set_state(gst::State::Null);
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(1));
            let (_, current, pending) = self
                .pipeline
                .state(gst::ClockTime::from_nseconds(wait.as_nanos() as u64));
            if current == gst::State::Null && pending == gst::State::VoidPending {
                self.terminal = true;
                if worker_panicked {
                    bail!("RTP worker panicked (pipeline stopped)");
                }
                return Ok(());
            }
        }
        bail!("RTP pipeline did not settle at NULL")
    }
    pub fn stats(&self) -> serde_json::Value {
        let c = &self.counters;
        let j = self.jitter.property::<gst::Structure>("stats");
        let age = c
            .last_packet
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_millis() as u64);
        serde_json::json!({"received":c.accepted.load(Ordering::Relaxed),
            "last_sequence":if c.accepted.load(Ordering::Relaxed)>0 {Some(c.last_sequence.load(Ordering::Relaxed))}else{None},
            "jitter_ms":j.get::<u64>("avg-jitter").unwrap_or(0) as f64 / 1_000_000.0,
            "packets_accepted":c.accepted.load(Ordering::Relaxed),"packets_rejected":c.rejected.load(Ordering::Relaxed),
            "decoded_buffers":c.decoded.load(Ordering::Relaxed),"output_buffers":c.output.load(Ordering::Relaxed),
            "socket_errors":c.socket_errors.load(Ordering::Relaxed),"push_errors":c.push_errors.load(Ordering::Relaxed),
            "last_packet_age_ms":age,"buffer_ms":self.buffer_ms,"ready":self.reported && !self.terminal,
            "lost":j.get::<u64>("num-lost").unwrap_or(0),"late":j.get::<u64>("num-late").unwrap_or(0),
            "duplicates":j.get::<u64>("num-duplicates").unwrap_or(0),"jitter_ns":j.get::<u64>("avg-jitter").unwrap_or(0)})
    }
    pub fn poll(&mut self) -> Vec<Event> {
        if self.terminal {
            return Vec::new();
        }
        let mut failure = None;
        while let Some(m) = self.bus.pop() {
            match m.view() {
                gst::MessageView::Error(_) => failure = Some("RTP decoder/output failure"),
                gst::MessageView::Eos(_) => failure = Some("RTP input ended"),
                gst::MessageView::Latency(_) => {
                    let _ = self.pipeline.recalculate_latency();
                }
                _ => {}
            }
        }
        let timeout = Duration::from_millis(u64::from(self.buffer_ms) + 3000);
        let last = self
            .counters
            .last_output
            .lock()
            .unwrap()
            .unwrap_or(self.started);
        if last.elapsed() > timeout {
            failure = Some("RTP output starvation");
        }
        if self.counters.socket_errors.load(Ordering::Relaxed) > 0
            || self.counters.push_errors.load(Ordering::Relaxed) > 0
        {
            failure = Some("RTP input failure");
        }
        if let Some(error) = failure {
            let _ = self.stop();
            self.terminal = true;
            return vec![Event::Error(error.into())];
        }
        let (_, state, pending) = self.pipeline.state(gst::ClockTime::ZERO);
        if !self.reported
            && self.counters.decoded.load(Ordering::Relaxed) > 0
            && self.counters.output.load(Ordering::Relaxed) > 0
            && state == gst::State::Playing
            && pending == gst::State::VoidPending
        {
            self.reported = true;
            return vec![Event::Playing];
        }
        Vec::new()
    }
}
impl Drop for RtpPlayback {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn format() -> RtpFormat {
        serde_json::from_value(serde_json::json!({"codec":"sbc","sample_rate":44100,"channels":2,"rtp":{"encoding_name":"SBC","payload_type":96,"ssrc":1,"clock_rate":44100}})).unwrap()
    }
    #[test]
    fn aac_rtp_fixture_decodes_and_empty_live_input_never_plays() {
        gst::init().unwrap();
        let f: RtpFormat = serde_json::from_value(serde_json::json!({
            "codec":"aac-lc","sample_rate":44100,"channels":2,
            "rtp":{"encoding_name":"MPEG4-GENERIC","payload_type":96,"ssrc":1,"clock_rate":44100,
                "fmtp":{"mode":"AAC-hbr","config":"1210","size_length":13,"index_length":3,"index_delta_length":3}}
        })).unwrap();
        let (mut receiver, port) = RtpPlayback::start(
            &f,
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            500,
            "unused",
            true,
        )
        .unwrap();
        assert!(!receiver.poll().iter().any(|e| matches!(e, Event::Playing)));
        let invalid = UdpSocket::bind("127.0.0.1:0").unwrap();
        invalid
            .send_to(&[0u8; 12], (std::net::Ipv4Addr::LOCALHOST, port))
            .unwrap();
        let sender = gst::parse::launch(&format!("audiotestsrc is-live=true num-buffers=100 ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! avenc_aac ! aacparse ! rtpmp4gpay pt=96 ssrc=1 ! udpsink host=127.0.0.1 port={port} sync=true")).unwrap();
        sender.set_state(gst::State::Playing).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut playing = false;
        while Instant::now() < deadline {
            for e in receiver.poll() {
                match e {
                    Event::Playing => playing = true,
                    Event::Error(e) => panic!("{e}"),
                    _ => {}
                }
            }
            if playing {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        sender.set_state(gst::State::Null).unwrap();
        assert!(
            playing,
            "real AAC RTP fixture must produce decoded output: {}",
            receiver.stats()
        );
        assert!(receiver.stats()["decoded_buffers"].as_u64().unwrap() > 0);
        assert_eq!(receiver.stats()["packets_rejected"], 1);
        // Missing output reaches a bounded terminal error, never permanent fake Playing.
        receiver.pipeline.set_state(gst::State::Paused).unwrap();
        let (_, current, _) = receiver.pipeline.state(gst::ClockTime::from_seconds(1));
        assert_eq!(current, gst::State::Paused);
        *receiver.counters.last_output.lock().unwrap() =
            Some(Instant::now() - Duration::from_secs(10));
        assert!(receiver.poll().iter().any(|e| matches!(e, Event::Error(_))));
        assert!(!receiver.stats()["ready"].as_bool().unwrap());
        receiver.stop().unwrap();
        assert_eq!(receiver.pipeline.current_state(), gst::State::Null);
        assert!(receiver.worker.is_none());
        // The old socket is closed synchronously, not retained by an old generation.
        let rebound = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        drop(rebound);
    }
    #[test]
    fn sbc_rtp_fixture_decodes() {
        gst::init().unwrap();
        let missing = ["sbcenc", "sbcparse", "sbcdec", "rtpsbcpay"]
            .into_iter()
            .any(|name| gst::ElementFactory::find(name).is_none());
        if missing {
            assert!(
                std::env::var_os("RADIOD_REQUIRE_RTP_CODECS").is_none(),
                "SBC plugins required for release validation"
            );
            eprintln!(
                "SBC fixture unavailable: host lacks SBC plugins; release CI must set RADIOD_REQUIRE_RTP_CODECS=1"
            );
            return;
        }
        let (mut receiver, port) = RtpPlayback::start(
            &format(),
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            500,
            "unused",
            true,
        )
        .unwrap();
        let sender = gst::parse::launch(&format!("audiotestsrc is-live=true num-buffers=100 ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! sbcenc ! rtpsbcpay pt=96 ssrc=1 ! udpsink host=127.0.0.1 port={port} sync=true")).unwrap();
        sender.set_state(gst::State::Playing).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut playing = false;
        while Instant::now() < deadline {
            for e in receiver.poll() {
                match e {
                    Event::Playing => playing = true,
                    Event::Error(e) => panic!("{e}"),
                    _ => {}
                }
            }
            if playing {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        sender.set_state(gst::State::Null).unwrap();
        assert!(
            playing,
            "real SBC RTP fixture must decode: {}",
            receiver.stats()
        );
        receiver.stop().unwrap();
        assert_eq!(receiver.pipeline.current_state(), gst::State::Null);
    }
    #[test]
    fn validates_format() {
        let mut f = format();
        f.validate().unwrap();
        f.rtp.ssrc = 0;
        assert!(f.validate().is_err());
        f.rtp.ssrc = 1;
        f.rtp.clock_rate = 48000;
        assert!(f.validate().is_err());
    }
    #[test]
    fn header_bounds_and_identity() {
        let f = format();
        let mut p = vec![0x80, 96, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1];
        assert!(valid_packet(&p, &f));
        p[0] = 0x90;
        assert!(!valid_packet(&p, &f));
        p[0] = 0xa0;
        p[12] = 0;
        assert!(!valid_packet(&p, &f));
        p[12] = 2;
        assert!(!valid_packet(&p, &f));
        p[0] = 0x80;
        p.resize(1513, 1);
        assert!(!valid_packet(&p, &f));
        p.truncate(13);
        p[11] = 0;
        assert!(!valid_packet(&p, &f));
        p[11] = 1;
        p[1] = 97;
        assert!(!valid_packet(&p, &f));
    }
}
