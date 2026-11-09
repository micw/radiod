//! End-to-end, wall-clock latency regression. No Core, discovery, or audio hardware.
use super::*;
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct ClockedWav {
    url: String,
    stop: Arc<AtomicBool>,
    first_sample: Arc<Mutex<Option<Instant>>>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl ClockedWav {
    fn start() -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/live.wav", listener.local_addr()?);
        let stop = Arc::new(AtomicBool::new(false));
        let first_sample = Arc::new(Mutex::new(None));
        let stopping = stop.clone();
        let origin = first_sample.clone();
        let worker = thread::spawn(move || {
            let result = serve(listener, &stopping, &origin);
            if stopping.load(Ordering::Acquire) {
                Ok(())
            } else {
                result
            }
        });
        Ok(Self {
            url,
            stop,
            first_sample,
            worker: Some(worker),
        })
    }

    fn finish(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("clocked WAV server thread panicked"))),
            None => Ok(()),
        }
    }
}

impl Drop for ClockedWav {
    fn drop(&mut self) {
        // accept/read/write all have bounded waits; even assertion unwinds join.
        let _ = self.finish();
    }
}

fn serve(
    listener: TcpListener,
    stop: &AtomicBool,
    first_sample: &Mutex<Option<Instant>>,
) -> io::Result<()> {
    let mut socket: TcpStream = loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    };
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    socket.set_write_timeout(Some(Duration::from_millis(250)))?;
    let request_deadline = Instant::now() + Duration::from_secs(2);
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if Instant::now() >= request_deadline || request.len() >= 16384 {
            return Err(io::Error::other("incomplete HTTP request"));
        }
        let mut byte = [0];
        match socket.read(&mut byte) {
            Ok(0) => return Err(io::Error::other("HTTP client closed before request")),
            Ok(_) => request.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    // AriaCast-style indefinite RIFF: both sizes unknown, no Content-Length.
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n")?;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&48000u32.to_le_bytes());
    header.extend_from_slice(&192000u32.to_le_bytes());
    header.extend_from_slice(&4u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&u32::MAX.to_le_bytes());
    socket.write_all(&header)?;
    // Exactly 960 native stereo frames per chunk = 20 ms. PTS zero refers to
    // this instant, NOT HTTP accept or header generation. No initial burst.
    let start = Instant::now();
    *unpoison(first_sample) = Some(start);
    let mut frame = 0u64;
    let mut pcm = [0u8; 960 * 4];
    while !stop.load(Ordering::Acquire) {
        let deadline = start + Duration::from_nanos(frame * 1_000_000_000 / 48000);
        while Instant::now() < deadline {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(10)),
            );
        }
        // Deterministic non-silent PCM; generation time is the native frame clock.
        for (index, stereo) in pcm.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let sample = if (frame + index as u64) % 96 < 48 {
                2048i16
            } else {
                -2048i16
            };
            stereo[..2].copy_from_slice(&sample.to_le_bytes());
            stereo[2..].copy_from_slice(&sample.to_le_bytes());
        }
        socket.write_all(&pcm)?;
        frame += 960;
        // Never catch up with a burst after a stalled write: report invalid
        // source pacing rather than manufacture a misleading latency result.
        if Instant::now()
            > start
                + Duration::from_nanos(frame * 1_000_000_000 / 48000)
                + Duration::from_millis(100)
        {
            return Err(io::Error::other(
                "clocked WAV source fell over 100ms behind real time",
            ));
        }
    }
    Ok(())
}

#[derive(Default)]
struct Samples {
    first_age_ms: Option<f64>,
    steady_age_ms: Vec<f64>,
    missing_pts: usize,
}

#[derive(Debug)]
struct Summary {
    first_ms: f64,
    median_ms: f64,
    min_ms: f64,
    p95_ms: f64,
    worst_ms: f64,
    samples: usize,
}

fn measure(external: bool) -> Result<Summary> {
    let mut source = ClockedWav::start()?;
    let mut audio = Audio::new("unused", "unused", "unused", true)?;
    let sink = audio.pipeline.property::<gst::Element>("audio-sink");
    assert!(
        sink.property::<bool>("sync"),
        "latency measurement requires clocked output"
    );
    sink.set_property("signal-handoffs", true);
    let samples = Arc::new(Mutex::new(Samples::default()));
    let captured = samples.clone();
    let first_sample = source.first_sample.clone();
    // GObject signal closure: no unwrap/assert/panic may cross its FFI boundary.
    sink.connect("handoff", false, move |values| {
        let now = Instant::now();
        let start = (*unpoison(&first_sample))?;
        let buffer = values
            .get(1)
            .and_then(|value| value.get::<gst::Buffer>().ok())?;
        let Some(pts) = buffer.pts() else {
            unpoison(&captured).missing_pts += 1;
            return None;
        };
        let elapsed = now.saturating_duration_since(start).as_secs_f64();
        let age_ms = (elapsed - pts.nseconds() as f64 / 1e9) * 1000.0;
        let mut samples = unpoison(&captured);
        if samples.first_age_ms.is_none() {
            samples.first_age_ms = Some(age_ms);
        }
        if elapsed >= 2.0 {
            samples.steady_age_ms.push(age_ms);
        }
        None
    });
    if external {
        audio.play_external(&source.url)?;
    } else {
        audio.play(&source.url)?;
    }
    let started = Instant::now();
    let deadline = started + Duration::from_millis(4500);
    let mut next_poll = started;
    let mut playback_error = None;
    let mut diagnosed = false;
    while Instant::now() < deadline {
        if !diagnosed && started.elapsed() >= Duration::from_secs(3) {
            diagnose_pipeline(&audio.pipeline);
            diagnosed = true;
        }
        for event in audio.poll() {
            match event {
                Event::Error(error) => playback_error = Some(error),
                Event::Ended => playback_error = Some("unexpected EOS on indefinite WAV".into()),
                _ => {}
            }
        }
        if playback_error.is_some() {
            break;
        }
        // Absolute monotonic 50ms cadence, not accumulated relative sleeps.
        next_poll += Duration::from_millis(50);
        while next_poll <= Instant::now() {
            next_poll += Duration::from_millis(50);
        }
        thread::sleep(
            next_poll
                .min(deadline)
                .saturating_duration_since(Instant::now()),
        );
    }
    source.stop.store(true, Ordering::Release);
    let stopped = audio.stop();
    let served = source.finish();
    stopped?;
    served?;
    if let Some(error) = playback_error {
        bail!("latency playback failed: {error}");
    }
    let mut samples = unpoison(&samples);
    let first_ms = samples
        .first_age_ms
        .context("no fakesink handoff with a valid PTS")?;
    assert_eq!(samples.missing_pts, 0, "sink delivered buffers without PTS");
    assert!(
        samples.steady_age_ms.len() >= 20,
        "insufficient steady-state sink output: {}",
        samples.steady_age_ms.len()
    );
    samples.steady_age_ms.sort_by(f64::total_cmp);
    let ages = &samples.steady_age_ms;
    let summary = Summary {
        first_ms,
        median_ms: ages[ages.len() / 2],
        min_ms: ages[0],
        p95_ms: ages[(ages.len() - 1) * 95 / 100],
        worst_ms: ages[ages.len() - 1],
        samples: ages.len(),
    };
    eprintln!(
        "clocked WAV {}: first={:.1}ms steady median={:.1}ms min={:.1}ms p95={:.1}ms worst={:.1}ms n={}",
        if external { "external" } else { "radio" },
        summary.first_ms,
        summary.median_ms,
        summary.min_ms,
        summary.p95_ms,
        summary.worst_ms,
        summary.samples
    );
    Ok(summary)
}

fn diagnose_pipeline(pipeline: &gst::Element) {
    let Some(bin) = pipeline.downcast_ref::<gst::Bin>() else {
        return;
    };
    let mut elements = vec![pipeline.clone()];
    let mut iterator = bin.iterate_recurse();
    while let Ok(Some(element)) = iterator.next() {
        elements.push(element);
    }
    for element in elements {
        let factory = element
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_default();
        let mut properties = Vec::new();
        for name in [
            "current-level-time",
            "current-level-bytes",
            "current-level-buffers",
            "max-size-time",
            "max-size-bytes",
            "max-size-buffers",
            "buffer-duration",
            "buffer-size",
            "low-watermark",
            "high-watermark",
            "low-percent",
            "high-percent",
            "is-live",
            "use-buffering",
            "min-threshold-time",
            "latency",
            "buffer-time",
        ] {
            if element.find_property(name).is_some() {
                properties.push(format!("{name}={:?}", element.property_value(name)));
            }
        }
        if !properties.is_empty() {
            eprintln!(
                "latency pipeline {factory}/{}: {}",
                element.name(),
                properties.join(" ")
            );
        }
    }
}

#[test]
fn clocked_http_wav_external_has_materially_lower_output_frame_age() -> Result<()> {
    let radio = measure(false)?;
    let external = measure(true)?;
    assert!(
        radio.median_ms >= 700.0,
        "radio buffering is not effective (expected >=700ms output frame age); investigate actual pipeline, do not weaken the measurement: {radio:?}"
    );
    assert!(
        external.median_ms < 600.0,
        "external output frame age exceeds 600ms: {external:?}"
    );
    assert!(
        radio.median_ms - external.median_ms >= 400.0,
        "external profile must improve actual output frame age by >=400ms: radio={radio:?}, external={external:?}"
    );
    Ok(())
}
