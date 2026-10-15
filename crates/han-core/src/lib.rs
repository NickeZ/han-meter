#![no_std]
#![forbid(unsafe_code)]

pub mod discovery;
pub mod meter;
pub mod parser;
pub mod state;

pub use meter::{Decimal, FIELD_COUNT, FIELDS, Field, Snapshot, Value};
pub use parser::{DecodeError, Decoder, MAX_TELEGRAM_LEN, crc16, parse_telegram};
pub use state::{Backoff, STALE_MS, Telemetry};
