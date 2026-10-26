#![no_std]
#![forbid(unsafe_code)]

pub mod http;
pub mod ntp;
#[cfg(feature = "tls")]
pub mod tls;

use base64::{Engine, engine::general_purpose::STANDARD};
use core::{fmt::Write, net::IpAddr};
use han_config::diagnostics::Page;
use heapless::String;
use serde::Serialize;

pub const BATCH_BYTES: usize = 6144;
pub const HEADER_BYTES: usize = 2048;

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    Url,
    Username,
    Password,
}

/// A validated HTTPS destination. Credentials are kept out of URLs and Debug.
pub struct Endpoint<'a> {
    authority: &'a str,
    host: &'a str,
    port: u16,
    path: &'a str,
}

impl<'a> Endpoint<'a> {
    pub fn host(&self) -> &str {
        self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn path(&self) -> &str {
        self.path
    }
    pub fn authority(&self) -> &str {
        self.authority
    }
    pub fn parse(url: &'a str) -> Result<Self, ConfigError> {
        let rest = url.strip_prefix("https://").ok_or(ConfigError::Url)?;
        let (authority, suffix) = rest.split_once('/').ok_or(ConfigError::Url)?;
        let path = &rest[authority.len()..];
        if authority.is_empty()
            || authority.contains('@')
            || path.len() > 768
            || suffix.contains('#')
            || !url.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            return Err(ConfigError::Url);
        }
        let (host, port) = if let Some(ipv6) = authority.strip_prefix('[') {
            let (host, tail) = ipv6.split_once(']').ok_or(ConfigError::Url)?;
            if !matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
                return Err(ConfigError::Url);
            }
            (
                host,
                tail.strip_prefix(':')
                    .or_else(|| tail.is_empty().then_some("443")),
            )
        } else if let Some((host, port)) = authority.split_once(':') {
            (host, Some(port))
        } else {
            (authority, Some("443"))
        };
        let port = port
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|p| *p != 0)
            .ok_or(ConfigError::Url)?;
        if host.is_empty()
            || host.len() > 253
            || (host.parse::<IpAddr>().is_err()
                && !host.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                }))
        {
            return Err(ConfigError::Url);
        }
        Ok(Self {
            authority,
            host,
            port,
            path,
        })
    }
}

/// Encode at build time; never put the resulting header in diagnostic output.
pub fn basic_auth(username: &str, password: &str) -> Result<String<522>, ConfigError> {
    if username.is_empty()
        || username.len() > 128
        || username.contains(':')
        || username.chars().any(char::is_control)
    {
        return Err(ConfigError::Username);
    }
    if password.is_empty() || password.len() > 256 || password.chars().any(char::is_control) {
        return Err(ConfigError::Password);
    }
    let mut plain = String::<385>::new();
    write!(plain, "{username}:{password}").map_err(|_| ConfigError::Password)?;
    let mut encoded = [0; 516];
    let len = STANDARD
        .encode_slice(plain.as_bytes(), &mut encoded)
        .map_err(|_| ConfigError::Password)?;
    let mut result = String::<522>::new();
    result
        .push_str("Basic ")
        .map_err(|_| ConfigError::Password)?;
    result
        .push_str(core::str::from_utf8(&encoded[..len]).map_err(|_| ConfigError::Password)?)
        .map_err(|_| ConfigError::Password)?;
    Ok(result)
}

pub struct Identity<'a> {
    pub device: &'a str,
    pub boot_id: u32,
    pub firmware: &'a str,
}

#[derive(Serialize)]
struct Line<'a> {
    _msg: &'a str,
    // VictoriaLogs uses ingestion time for 0; keep uptime for ordering regardless.
    _time: u64,
    device: &'a str,
    boot_id: u32,
    firmware: &'a str,
    sequence: u64,
    uptime_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    lost_entries: Option<u64>,
}

/// A batch owns its bytes until acknowledged. Retrying preserves IDs and content.
/// `boot_unix_ms` is estimated from the first successful time synchronization.
pub fn encode_batch(
    page: &Page,
    after: u64,
    identity: &Identity<'_>,
    boot_unix_ms: Option<u64>,
    out: &mut [u8],
) -> Result<usize, serde_json_core::ser::Error> {
    let mut used = 0;
    for entry in &page.entries {
        let lost = entry.sequence.saturating_sub(after.saturating_add(1));
        let line = Line {
            _msg: &entry.message,
            _time: boot_unix_ms
                .and_then(|base| base.checked_add(entry.uptime_ms))
                .unwrap_or(0),
            device: identity.device,
            boot_id: identity.boot_id,
            firmware: identity.firmware,
            sequence: entry.sequence,
            uptime_ms: entry.uptime_ms,
            lost_entries: (page.lost && used == 0 && lost > 0).then_some(lost),
        };
        used += serde_json_core::to_slice(&line, &mut out[used..])?;
        let next = out
            .get_mut(used)
            .ok_or(serde_json_core::ser::Error::BufferFull)?;
        *next = b'\n';
        used += 1;
    }
    Ok(used)
}
