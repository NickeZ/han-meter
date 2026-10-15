#![no_std]
#![forbid(unsafe_code)]

pub mod control;
pub mod storage;
#[cfg(feature = "web")]
pub mod web;

use core::fmt;
use heapless::String;

/// Owned MQTT credentials. Deliberately has no Serialize or Debug implementation.
#[derive(Clone, PartialEq, Eq)]
pub struct MqttSettings {
    pub host: String<253>,
    pub port: u16,
    pub username: String<128>,
    pub password: String<256>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationError {
    Host,
    Port,
    Username,
    Password,
}

impl ValidationError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Host => {
                "Host must be an IPv4 address or DNS hostname (1–253 bytes), without a scheme or port."
            }
            Self::Port => "Port must be between 1 and 65535.",
            Self::Username => "Username must contain 1–128 bytes and no control characters.",
            Self::Password => "Password must contain 1–256 bytes and no control characters.",
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl MqttSettings {
    pub fn new(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
    ) -> Result<Self, ValidationError> {
        if host.is_empty()
            || host.len() > 253
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            return Err(ValidationError::Host);
        }
        if port == 0 {
            return Err(ValidationError::Port);
        }
        let text_ok = |text: &str, max| {
            !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
        };
        if !text_ok(username, 128) {
            return Err(ValidationError::Username);
        }
        if !text_ok(password, 256) {
            return Err(ValidationError::Password);
        }
        Ok(Self {
            host: host.try_into().map_err(|_| ValidationError::Host)?,
            port,
            username: username.try_into().map_err(|_| ValidationError::Username)?,
            password: password.try_into().map_err(|_| ValidationError::Password)?,
        })
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        Self::new(&self.host, self.port, &self.username, &self.password).map(|_| ())
    }
}
