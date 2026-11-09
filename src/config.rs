use anyhow::{Context, Result, bail};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BluetoothConfig {
    pub enabled: bool,
    pub listen: SocketAddr,
    pub rtp_listen: SocketAddr,
    pub advertise_address: Option<IpAddr>,
    pub allowed_networks: Vec<IpNet>,
    pub allowed_devices: Vec<String>,
    pub buffer_ms: u32,
    pub lease_ms: u64,
    pub suspend_ms: u64,
}
impl Default for BluetoothConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:8092".parse().unwrap(),
            rtp_listen: "0.0.0.0:5004".parse().unwrap(),
            advertise_address: None,
            allowed_networks: Vec::new(),
            allowed_devices: Vec::new(),
            buffer_ms: 500,
            lease_ms: 6000,
            suspend_ms: 2500,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct UpnpConfig {
    pub enabled: bool,
    pub bind_address: Ipv4Addr,
    pub port: u16,
    pub interface: Option<String>,
    pub friendly_name: String,
    pub allowed_networks: Vec<IpNet>,
    pub prepared_timeout_seconds: u64,
    pub failure_timeout_seconds: u64,
}
impl Default for UpnpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: Ipv4Addr::LOCALHOST,
            port: 8091,
            interface: None,
            friendly_name: "radiod".into(),
            allowed_networks: Vec::new(),
            prepared_timeout_seconds: 15,
            failure_timeout_seconds: 10,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Station {
    pub name: String,
    pub url: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: String,
    pub alsa_device: String,
    pub mixer_device: String,
    pub mixer_control: String,
    pub initial_volume: u8,
    pub retry_max_seconds: u64,
    pub connect_timeout_seconds: u64,
    pub autostart: bool,
    pub stations: Vec<Station>,
    pub upnp: UpnpConfig,
    pub bluetooth: BluetoothConfig,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:6600".into(),
            alsa_device: "default".into(),
            mixer_device: "default".into(),
            mixer_control: "Master".into(),
            initial_volume: 25,
            retry_max_seconds: 30,
            connect_timeout_seconds: 20,
            autostart: false,
            stations: vec![],
            upnp: UpnpConfig::default(),
            bluetooth: BluetoothConfig::default(),
        }
    }
}
impl Config {
    pub fn from_bytes(text: &[u8]) -> Result<Self> {
        let c: Self = serde_yaml::from_slice(text).context("parse configuration")?;
        if c.stations.len() > 1024 {
            bail!("at most 1024 stations allowed");
        }
        if c.initial_volume > 100 {
            bail!("initial_volume must be between 0 and 100");
        }
        if !(1..=3600).contains(&c.retry_max_seconds)
            || !(1..=3600).contains(&c.connect_timeout_seconds)
        {
            bail!("timeouts must be between 1 and 3600 seconds");
        }
        if c.upnp.enabled {
            if c.upnp.port == 0 {
                bail!("UPnP requires a nonzero HTTP port");
            }
            if c.upnp.bind_address.is_unspecified()
                || c.upnp.bind_address.is_multicast()
                || c.upnp.bind_address.is_broadcast()
            {
                bail!("UPnP needs a concrete unicast IPv4 bind address");
            }
            if c.upnp.allowed_networks.is_empty() {
                bail!("UPnP requires explicit allowed_networks for controllers and media sources");
            }
            if c.upnp.friendly_name.is_empty()
                || c.upnp.friendly_name.len() > 128
                || c.upnp.friendly_name.chars().any(char::is_control)
            {
                bail!("invalid UPnP friendly_name");
            }
            if c.upnp.interface.as_ref().is_some_and(|s| {
                s.is_empty()
                    || s.len() > 15
                    || !s
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "_.:-".contains(ch))
            }) {
                bail!("invalid UPnP interface");
            }
            if !(1..=3600).contains(&c.upnp.prepared_timeout_seconds)
                || !(1..=3600).contains(&c.upnp.failure_timeout_seconds)
            {
                bail!("UPnP timeouts must be between 1 and 3600 seconds");
            }
        }
        if c.bluetooth.enabled {
            let b = &c.bluetooth;
            if b.listen.port() == 0 || b.rtp_listen.port() == 0 {
                bail!("Bluetooth control and RTP require nonzero ports");
            }
            if b.listen.is_ipv4() != b.rtp_listen.is_ipv4()
                || b.advertise_address
                    .is_some_and(|a| a.is_ipv4() != b.rtp_listen.is_ipv4())
            {
                bail!(
                    "Bluetooth control, RTP and advertised addresses must use the same IP family"
                );
            }
            if b.allowed_networks.is_empty() {
                bail!("Bluetooth requires explicit allowed_networks (not authentication)");
            }
            if b.advertise_address.is_none_or(|a| {
                a.is_unspecified() || a.is_multicast() || a == IpAddr::V4(Ipv4Addr::BROADCAST)
            }) {
                bail!("Bluetooth requires a reachable unicast advertise_address");
            }
            if !(50..=5000).contains(&b.buffer_ms)
                || !(1000..=60000).contains(&b.lease_ms)
                || b.suspend_ms > 10000
            {
                bail!("invalid Bluetooth buffer or lease/suspend interval");
            }
            if b.allowed_devices.len() > 32
                || b.allowed_devices.iter().any(|d| {
                    d.is_empty()
                        || d.len() > 64
                        || !d
                            .bytes()
                            .all(|v| v.is_ascii_alphanumeric() || b"_.-".contains(&v))
                })
            {
                bail!("invalid Bluetooth allowed_devices");
            }
            if c.listen.parse::<SocketAddr>().ok() == Some(b.listen) {
                bail!("MPD and Bluetooth control listeners must differ");
            }
        }
        let mut urls = HashSet::new();
        for s in &c.stations {
            if s.name.trim().is_empty()
                || s.name.len() > 256
                || s.url.len() > 4096
                || s.name.chars().any(char::is_control)
                || s.url.chars().any(|c| c.is_control() || c.is_whitespace())
                || !(s.url.starts_with("http://") || s.url.starts_with("https://"))
                || !urls.insert(&s.url)
            {
                bail!("invalid station or duplicate URL: {}", s.name);
            }
        }
        Ok(c)
    }
}
pub fn read_resource(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .context("open configuration")?
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        bail!("configuration exceeds 1 MiB");
    }
    Ok(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_safe() {
        let c = Config::default();
        assert!(c.initial_volume <= 100);
        assert!(!c.autostart);
    }
    #[test]
    fn duplicate_urls_rejected_but_duplicate_names_allowed() {
        assert!(Config::from_bytes(b"stations:\n - {name: A, url: 'https://radio/a'}\n - {name: A, url: 'https://radio/a'}\n").is_err());
        assert!(Config::from_bytes(b"stations:\n - {name: A, url: 'https://radio/a'}\n - {name: A, url: 'https://radio/b'}\n").is_ok());
    }
    #[test]
    fn unreasonable_timeouts_and_volume_rejected() {
        assert!(Config::from_bytes(b"retry_max_seconds: 18446744073709551615").is_err());
        assert!(Config::from_bytes(b"initial_volume: 101").is_err());
        assert!(Config::from_bytes(b"initial_volume: -1").is_err());
        assert!(Config::from_bytes(b"initial_volume: 100").is_ok());
        assert!(Config::from_bytes(b"connect_timeout_seconds: 0").is_err());
    }
    #[test]
    fn bluetooth_requires_explicit_reachable_network_policy_and_bounded_static_buffer() {
        assert!(Config::from_bytes(b"bluetooth: {enabled: true}").is_err());
        assert!(Config::from_bytes(b"bluetooth: {enabled: true, advertise_address: 127.0.0.1, allowed_networks: ['127.0.0.0/8']}").is_ok());
        for extra in [
            "buffer_ms: 0",
            "buffer_ms: 5001",
            "lease_ms: 0",
            "suspend_ms: 10001",
            "listen: '127.0.0.1:0'",
            "rtp_listen: '[::]:5004'",
            "allowed_devices: ['bad/device']",
        ] {
            let text = format!(
                "bluetooth:\n  enabled: true\n  advertise_address: 127.0.0.1\n  allowed_networks: ['127.0.0.0/8']\n  {extra}\n"
            );
            assert!(Config::from_bytes(text.as_bytes()).is_err(), "{extra}");
        }
    }
    #[test]
    fn upnp_requires_explicit_valid_network_settings() {
        assert!(Config::from_bytes(b"upnp: {enabled: true}").is_err());
        assert!(
            Config::from_bytes(b"upnp: {enabled: true, allowed_networks: ['127.0.0.0/8']}").is_ok()
        );
        for text in [
            "upnp: {enabled: true, port: 0, allowed_networks: ['127.0.0.0/8']}",
            "upnp: {enabled: true, bind_address: 0.0.0.0, allowed_networks: ['127.0.0.0/8']}",
            "upnp: {enabled: true, interface: '../bad', allowed_networks: ['127.0.0.0/8']}",
            "upnp: {enabled: true, failure_timeout_seconds: 0, allowed_networks: ['127.0.0.0/8']}",
        ] {
            assert!(Config::from_bytes(text.as_bytes()).is_err(), "{text}");
        }
    }
    #[test]
    fn unknown_fields_rejected() {
        assert!(serde_yaml::from_str::<Config>("typo: 1").is_err());
        assert!(Config::from_bytes(b"max_volume: 70").is_err());
    }
}
