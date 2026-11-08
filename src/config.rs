use anyhow::{Context, Result, bail};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::Ipv4Addr, path::Path};

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
    pub max_volume: u8,
    pub retry_max_seconds: u64,
    pub connect_timeout_seconds: u64,
    pub autostart: bool,
    pub stations: Vec<Station>,
    pub upnp: UpnpConfig,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:6600".into(),
            alsa_device: "default".into(),
            mixer_device: "default".into(),
            mixer_control: "Master".into(),
            initial_volume: 25,
            max_volume: 70,
            retry_max_seconds: 30,
            connect_timeout_seconds: 20,
            autostart: false,
            stations: vec![],
            upnp: UpnpConfig::default(),
        }
    }
}
impl Config {
    pub fn from_bytes(text: &[u8]) -> Result<Self> {
        let c: Self = serde_yaml::from_slice(text).context("parse configuration")?;
        if c.stations.len() > 1024 {
            bail!("at most 1024 stations allowed");
        }
        if c.initial_volume > c.max_volume || c.max_volume > 100 {
            bail!("volume must satisfy initial <= maximum <= 100");
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
        assert!(c.initial_volume <= c.max_volume);
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
        assert!(Config::from_bytes(b"max_volume: 101").is_err());
        assert!(Config::from_bytes(b"connect_timeout_seconds: 0").is_err());
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
    }
}
