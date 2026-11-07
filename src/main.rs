mod audio;
mod config;
mod core;
mod mpd;
use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from("radiod.yaml");
    let mut state = PathBuf::from("state/state.json");
    let mut null = false;
    let mut check = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => path = args.next().context("missing config path")?.into(),
            "--state" => state = args.next().context("missing state path")?.into(),
            "--null-output" => null = true,
            "--check-config" => check = true,
            "--help" => {
                println!(
                    "radiod [--config radiod.yaml] [--state state/state.json] [--null-output] [--check-config]"
                );
                return Ok(());
            }
            _ => anyhow::bail!("unknown argument: {a}"),
        }
    }
    let mut content = config::read_resource(&path)?;
    let config = config::Config::from_bytes(&content)?;
    if check {
        println!("valid configuration: {} stations", config.stations.len());
        return Ok(());
    }
    let listener = std::net::TcpListener::bind(&config.listen).context("bind MPD listener")?;
    let audio = audio::Audio::new(
        &config.alsa_device,
        &config.mixer_device,
        &config.mixer_control,
        null,
    )?;
    let core = Arc::new(Mutex::new(core::Core::new(config, audio, state)?));
    let worker = core.clone();
    std::thread::spawn(move || {
        let mut n = 0u64;
        loop {
            std::thread::sleep(Duration::from_millis(50));
            worker.lock().unwrap().tick();
            n += 1;
            if n.is_multiple_of(20) {
                match config::read_resource(&path) {
                    Ok(new) if new != content => {
                        let candidate = config::Config::from_bytes(&new);
                        content = new;
                        match candidate.and_then(|c| worker.lock().unwrap().reload(c)) {
                            Ok(()) => eprintln!("configuration reloaded"),
                            Err(e) => eprintln!("configuration rejected: {e:#}"),
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if n.is_multiple_of(1200) {
                            eprintln!("configuration unavailable (retaining previous): {e}");
                        }
                    }
                }
            }
        }
    });
    eprintln!("radiod MPD listening on {}", listener.local_addr()?);
    mpd::serve(listener, core)
}
