use core::fmt::Write;
use heapless::String;
use serde::Serialize;

use crate::{Field, STALE_MS};

pub const DISCOVERY_CAPACITY: usize = 1536;
pub const HA_STATUS_TOPIC: &str = "homeassistant/status";

pub fn device_id(mac: [u8; 6]) -> String<24> {
    let mut id = String::new();
    id.push_str("han_").unwrap();
    for byte in mac {
        write!(id, "{byte:02x}").unwrap();
    }
    id
}

pub fn availability_topic(id: &str) -> Result<String<128>, core::fmt::Error> {
    let mut topic = String::new();
    write!(topic, "han_meter/{id}/availability")?;
    Ok(topic)
}

pub struct Topics {
    pub discovery: String<128>,
    pub state: String<128>,
    pub availability: String<128>,
    pub unique_id: String<96>,
}

impl Topics {
    pub fn new(id: &str, field: &Field) -> Result<Self, core::fmt::Error> {
        let mut result = Self {
            discovery: String::new(),
            state: String::new(),
            availability: String::new(),
            unique_id: String::new(),
        };
        write!(result.unique_id, "{id}_{}", field.key)?;
        write!(
            result.discovery,
            "homeassistant/sensor/{}/config",
            result.unique_id
        )?;
        write!(result.state, "han_meter/{id}/{}/state", field.key)?;
        write!(
            result.availability,
            "han_meter/{id}/{}/availability",
            field.key
        )?;
        Ok(result)
    }
}

#[derive(Serialize)]
struct Device<'a> {
    identifiers: [&'a str; 1],
    name: &'a str,
    model: &'a str,
    sw_version: &'a str,
}

#[derive(Serialize)]
struct Origin<'a> {
    name: &'a str,
    sw_version: &'a str,
}

#[derive(Serialize)]
struct Availability<'a> {
    topic: &'a str,
}

#[derive(Serialize)]
struct Sensor<'a> {
    name: &'a str,
    unique_id: &'a str,
    state_topic: &'a str,
    availability: [Availability<'a>; 2],
    availability_mode: &'a str,
    expire_after: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit_of_measurement: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_class: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_class: Option<&'a str>,
    device: Device<'a>,
    origin: Origin<'a>,
}

pub fn encode(
    id: &str,
    name: &str,
    version: &str,
    field: &Field,
    topics: &Topics,
    availability: &str,
    output: &mut [u8],
) -> Result<usize, serde_json_core::ser::Error> {
    serde_json_core::to_slice(
        &Sensor {
            name: field.name,
            unique_id: &topics.unique_id,
            state_topic: &topics.state,
            availability: [
                Availability {
                    topic: availability,
                },
                Availability {
                    topic: &topics.availability,
                },
            ],
            availability_mode: "all",
            expire_after: STALE_MS / 1000,
            unit_of_measurement: field.unit,
            device_class: field.device_class,
            state_class: field.state_class,
            device: Device {
                identifiers: [id],
                name,
                model: "ESP32-C3 Swedish HAN/P1",
                sw_version: version,
            },
            origin: Origin {
                name: "han-meter",
                sw_version: version,
            },
        },
        output,
    )
}
