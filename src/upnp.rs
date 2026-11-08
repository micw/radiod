//! GUPnP owns discovery/SOAP/GENA; all playback policy stays in Core.
use crate::{config::UpnpConfig, core::Core};
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    net::IpAddr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use url::{Host, Url};

#[repr(C)]
struct NativeConfig {
    interface_name: *const c_char,
    bind_address: *const c_char,
    friendly_name: *const c_char,
    uuid: *const c_char,
    asset_dir: *const c_char,
    http_port: u16,
}
#[repr(C)]
struct Snapshot {
    revision: u64,
    transport_state: c_int,
    transport_error: c_int,
    volume: c_int,
    mute: c_int,
    duration_seconds: u32,
    position_seconds: u32,
    uri: [c_char; 4097],
    metadata: [c_char; 8193],
}
#[repr(C)]
struct Callbacks {
    ready: unsafe extern "C" fn(*mut c_void, c_int, *const c_char),
    action: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
        c_int,
        *mut c_char,
        usize,
    ) -> c_int,
    snapshot: unsafe extern "C" fn(*mut c_void, *mut Snapshot),
    should_stop: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
}
unsafe extern "C" {
    fn radiod_upnp_run(
        config: *const NativeConfig,
        callbacks: *const Callbacks,
        userdata: *mut c_void,
        error: *mut c_char,
        error_len: usize,
    ) -> c_int;
}
struct Bridge {
    core: Arc<Mutex<Core>>,
    config: UpnpConfig,
    startup: mpsc::SyncSender<std::result::Result<(), String>>,
    shutdown: Arc<AtomicBool>,
}

fn device_uuid(state_path: &Path) -> Result<String> {
    let path = state_path.with_extension("upnp.uuid");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            return Ok(uuid::Uuid::parse_str(text.trim())
                .context("invalid persisted UPnP UUID")?
                .to_string());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("read persisted UPnP UUID"),
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let value = uuid::Uuid::new_v4().to_string();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .context("create UPnP device identity")?;
    file.write_all(value.as_bytes())?;
    file.sync_all()?;
    Ok(value)
}

/// Startup is checked before returning: a missing LAN interface must not silently
/// leave an apparently enabled renderer unreachable.
pub fn start(core: Arc<Mutex<Core>>, state_path: &Path) -> Result<()> {
    let config = core
        .lock()
        .map_err(|_| anyhow::anyhow!("state lock poisoned"))?
        .config
        .upnp
        .clone();
    if !config.enabled {
        return Ok(());
    }
    let id = device_uuid(state_path)?;
    let (tx, rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let shutdown = Arc::new(AtomicBool::new(false));
    let signal_flag = shutdown.clone();
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ])?;
    std::thread::Builder::new()
        .name("gupnp-renderer".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let interface = config
                    .interface
                    .as_ref()
                    .map(|s| CString::new(s.as_str()))
                    .transpose()?;
                let bind = CString::new(config.bind_address.to_string())?;
                let friendly = CString::new(config.friendly_name.as_str())?;
                let uuid = CString::new(id)?;
                // Ship the XML assets in the binary, not as editable state files.
                let directory =
                    std::env::temp_dir().join(format!("radiod-upnp-{}", std::process::id()));
                std::fs::create_dir_all(&directory)?;
                for (name, xml) in [
                    ("device.xml", include_str!("../native/upnp/device.xml")),
                    (
                        "AVTransport.xml",
                        include_str!("../native/upnp/AVTransport.xml"),
                    ),
                    (
                        "RenderingControl.xml",
                        include_str!("../native/upnp/RenderingControl.xml"),
                    ),
                    (
                        "ConnectionManager.xml",
                        include_str!("../native/upnp/ConnectionManager.xml"),
                    ),
                ] {
                    std::fs::write(directory.join(name), xml)?;
                }
                let assets = CString::new(directory.to_string_lossy().as_bytes())?;
                let native = NativeConfig {
                    interface_name: interface.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                    bind_address: bind.as_ptr(),
                    friendly_name: friendly.as_ptr(),
                    uuid: uuid.as_ptr(),
                    asset_dir: assets.as_ptr(),
                    http_port: config.port,
                };
                let bridge = Bridge {
                    core,
                    config,
                    startup: tx.clone(),
                    shutdown,
                };
                let callbacks = Callbacks {
                    ready,
                    action,
                    snapshot,
                    should_stop: Some(should_stop),
                };
                let mut error = [0 as c_char; 512];
                // The stack-owned config/bridge remain alive for the blocking native
                // loop. Native callbacks never retain pointers into Rust strings.
                let result = unsafe {
                    radiod_upnp_run(
                        &native,
                        &callbacks,
                        (&bridge as *const Bridge).cast_mut().cast(),
                        error.as_mut_ptr(),
                        error.len(),
                    )
                };
                if result != 0 {
                    bail!("GUPnP renderer stopped or failed to initialize");
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("UPnP renderer failed: {error:#}");
                let _ = tx.send(Err("UPnP renderer initialization failed".into()));
            }
            let _ = done_tx.send(());
        })?;
    std::thread::Builder::new()
        .name("upnp-shutdown".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                signal_flag.store(true, Ordering::Release);
                // Allow GUPnP to send ssdp:byebye while the LAN interface still
                // exists. Do not change saved radio intent or hardware volume.
                let _ = done_rx.recv_timeout(Duration::from_secs(5));
                std::process::exit(0);
            }
        })?;
    rx.recv_timeout(Duration::from_secs(10))
        .context("wait for UPnP startup")?
        .map_err(anyhow::Error::msg)?;
    Ok(())
}

unsafe extern "C" fn should_stop(data: *mut c_void) -> c_int {
    let bridge = unsafe { &*data.cast::<Bridge>() };
    c_int::from(bridge.shutdown.load(Ordering::Acquire))
}

unsafe extern "C" fn ready(data: *mut c_void, success: c_int, _message: *const c_char) {
    let bridge = unsafe { &*data.cast::<Bridge>() };
    let _ = bridge.startup.send(if success != 0 {
        Ok(())
    } else {
        Err(
            "GUPnP initialization failed; verify bind address/interface and configured HTTP port"
                .into(),
        )
    });
}
fn allowed(config: &UpnpConfig, address: IpAddr) -> bool {
    config
        .allowed_networks
        .iter()
        .any(|net| net.contains(&address))
}

fn validate_uri(config: &UpnpConfig, text: &str) -> Result<String> {
    ensure!(
        text.len() <= 4096 && !text.chars().any(|c| c.is_control() || c.is_whitespace()),
        "invalid media URI"
    );
    let uri = Url::parse(text).context("invalid media URI")?;
    ensure!(
        matches!(uri.scheme(), "http" | "https"),
        "only HTTP(S) media is allowed"
    );
    // Initial LAN capture PoC accepts numeric LAN hosts. This avoids arbitrary
    // filesystem/cluster/metadata-service access and DNS rebinding through UPnP.
    let address = match uri.host() {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        _ => bail!("this LAN PoC requires a numeric media host in allowed_networks"),
    };
    ensure!(
        allowed(config, address),
        "media host is outside allowed_networks"
    );
    Ok(uri.to_string())
}

fn metadata_fields(xml: &str) -> Result<(Option<String>, Option<String>)> {
    ensure!(xml.len() <= 8192, "media metadata is too large");
    if xml.is_empty() || xml == "NOT_IMPLEMENTED" {
        return Ok((None, None));
    }
    let doc = roxmltree::Document::parse(xml).context("invalid DIDL metadata")?;
    let text = |name: &str| -> Result<Option<String>> {
        let value = doc
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == name)
            .map(|n| n.text().unwrap_or(""));
        if let Some(v) = value {
            ensure!(
                v.len() <= 1024 && !v.chars().any(char::is_control),
                "invalid metadata field"
            );
        }
        Ok(value.map(str::to_owned))
    };
    Ok((text("title")?, text("artist")?.or(text("creator")?)))
}

unsafe fn input<'a>(text: *const c_char) -> Result<&'a str> {
    ensure!(!text.is_null(), "missing action argument");
    unsafe { CStr::from_ptr(text) }
        .to_str()
        .context("invalid action text")
}
fn write_buffer<const N: usize>(output: &mut [c_char; N], text: &str) {
    let mut length = text.len().min(N - 1);
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    for (target, byte) in output.iter_mut().zip(text.as_bytes().iter()).take(length) {
        *target = *byte as c_char;
    }
    output[length] = 0;
}
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn didl(uri: &str, title: &str, artist: Option<&str>) -> String {
    let creator = artist
        .map(|a| format!("<upnp:artist>{}</upnp:artist>", xml_escape(a)))
        .unwrap_or_default();
    let xml = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><item id=\"0\" parentID=\"-1\" restricted=\"1\"><dc:title>{}</dc:title>{creator}<upnp:class>object.item.audioItem.audioBroadcast</upnp:class><res protocolInfo=\"http-get:*:audio/*:*\">{}</res></item></DIDL-Lite>",
        xml_escape(title),
        xml_escape(uri)
    );
    // Never cut XML mid-element when escaping expands a long tokenized URI.
    if xml.len() > 8192 { String::new() } else { xml }
}

unsafe extern "C" fn action(
    data: *mut c_void,
    name: *const c_char,
    owner: *const c_char,
    uri: *const c_char,
    metadata: *const c_char,
    value: c_int,
    error: *mut c_char,
    error_len: usize,
) -> c_int {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let bridge = unsafe { &*data.cast::<Bridge>() };
        let name = unsafe { input(name) }?;
        let owner = unsafe { input(owner) }?;
        let address = owner
            .parse::<IpAddr>()
            .context("invalid controller address")?;
        ensure!(
            allowed(&bridge.config, address),
            "controller is outside allowed_networks"
        );
        let media = if name == "SetAVTransportURI" {
            let text = unsafe { input(uri) }?;
            let uri = if text.is_empty() {
                String::new()
            } else {
                validate_uri(&bridge.config, text)?
            };
            let (title, artist) = metadata_fields(unsafe { input(metadata) }?)?;
            Some((uri, title, artist))
        } else {
            None
        };
        let mut core = bridge
            .core
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        if core.external_owner().is_some_and(|active| active != owner) {
            bail!("external source belongs to another controller");
        }
        match name {
            "SetAVTransportURI" => {
                let (uri, title, artist) = media.unwrap();
                if uri.is_empty() {
                    core.external_stop(owner)?;
                } else {
                    core.prepare_external(owner, &uri, title, artist)?;
                }
            }
            "Play" => core.external_play(owner)?,
            "Pause" => core.external_pause(owner)?,
            "Stop" => core.external_stop(owner)?,
            "SetVolume" => {
                ensure!((0..=100).contains(&value), "invalid volume");
                core.upnp_set_volume(value as u8)?;
            }
            "SetMute" => {
                ensure!(value == 0 || value == 1, "invalid mute");
                core.set_mute(value != 0)?;
            }
            _ => bail!("unsupported action"),
        }
        Ok(())
    }));
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(_)) | Err(_) => {
            // Native/UPnP error responses must not leak URI tokens or local paths.
            let safe = b"Action rejected by radiod source policy\0";
            if !error.is_null() && error_len > 0 {
                let count = (safe.len() - 1).min(error_len - 1);
                unsafe {
                    std::ptr::copy_nonoverlapping(safe.as_ptr().cast::<c_char>(), error, count);
                    *error.add(count) = 0;
                }
            }
            701
        }
    }
}

unsafe extern "C" fn snapshot(data: *mut c_void, output: *mut Snapshot) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let bridge = unsafe { &*data.cast::<Bridge>() };
        let out = unsafe { &mut *output };
        if let Ok(core) = bridge.core.lock() {
            out.revision = core.revision;
            out.transport_state = match core.state.as_str() {
                "play" => 2,
                "pause" => 3,
                "buffering" | "reconnecting" => 1,
                _ => 0,
            };
            out.transport_error = c_int::from(core.error.is_some());
            out.volume = core.volume.into();
            out.mute = c_int::from(core.muted);
            out.duration_seconds = 0;
            out.position_seconds = 0;
            if let Some(song) = core.current.and_then(|id| core.song(id)) {
                write_buffer(&mut out.uri, &song.station.url);
                write_buffer(
                    &mut out.metadata,
                    &didl(
                        &song.station.url,
                        core.title.as_deref().unwrap_or(&song.station.name),
                        core.artist.as_deref(),
                    ),
                );
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> UpnpConfig {
        UpnpConfig {
            allowed_networks: vec!["127.0.0.0/8".parse().unwrap()],
            ..UpnpConfig::default()
        }
    }
    #[test]
    fn lan_uri_policy_rejects_other_schemes_and_hosts() {
        let c = config();
        assert!(validate_uri(&c, "http://127.0.0.1:9000/live").is_ok());
        for uri in [
            "file:///etc/passwd",
            "ftp://127.0.0.1/live",
            "http://10.43.0.1/",
            "http://localhost/live",
            "http://169.254.169.254/",
        ] {
            assert!(validate_uri(&c, uri).is_err(), "{uri}");
        }
    }
    #[test]
    fn didl_is_well_formed_and_metadata_cannot_expand_entities() {
        let xml = didl("http://127.0.0.1/?a=1&b=2", "A < B", Some("Artist & Co"));
        assert_eq!(
            metadata_fields(&xml).unwrap(),
            (Some("A < B".into()), Some("Artist & Co".into()))
        );
        assert!(
            metadata_fields("<!DOCTYPE x [<!ENTITY ext SYSTEM 'file:///etc/passwd'>]><x>&ext;</x>")
                .is_err()
        );
    }
    #[test]
    fn native_snapshot_layout_matches_header() {
        assert_eq!(std::mem::size_of::<Snapshot>(), 12328);
    }
}
