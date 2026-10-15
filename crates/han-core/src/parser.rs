use crate::{Decimal, FIELDS, Snapshot, Value};

pub const MAX_TELEGRAM_LEN: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Overflow,
    Framing,
    Checksum,
    InvalidValue,
    DuplicateField,
    NoReadings,
}

/// CRC-16/ARC: reflected polynomial 0xa001, initial value 0, no final xor.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in bytes {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xa001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

pub fn parse_telegram(frame: &[u8]) -> Result<Snapshot, DecodeError> {
    if frame.len() > MAX_TELEGRAM_LEN {
        return Err(DecodeError::Overflow);
    }
    if frame.len() < 12 || frame.first() != Some(&b'/') || !frame.ends_with(b"\r\n") {
        return Err(DecodeError::Framing);
    }
    let end = frame.len() - 7;
    if frame[end] != b'!' || !frame[..end].ends_with(b"\r\n") {
        return Err(DecodeError::Framing);
    }
    let hex = core::str::from_utf8(&frame[end + 1..end + 5]).map_err(|_| DecodeError::Checksum)?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DecodeError::Checksum);
    }
    let expected = u16::from_str_radix(hex, 16).map_err(|_| DecodeError::Checksum)?;
    if crc16(&frame[..=end]) != expected {
        return Err(DecodeError::Checksum);
    }
    let text = core::str::from_utf8(&frame[..end]).map_err(|_| DecodeError::Framing)?;
    let mut lines = text.split("\r\n");
    let header = lines.next().ok_or(DecodeError::Framing)?;
    if header.len() < 4 || !header.bytes().all(|b| (b' '..=b'~').contains(&b)) {
        return Err(DecodeError::Framing);
    }
    let mut snapshot = Snapshot::default();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if !line.bytes().all(|b| (b' '..=b'~').contains(&b)) {
            return Err(DecodeError::Framing);
        }
        let (obis, rest) = line.split_once('(').ok_or(DecodeError::Framing)?;
        let Some(index) = FIELDS.iter().position(|f| f.obis == obis) else {
            continue;
        };
        if snapshot.readings[index].is_some() {
            return Err(DecodeError::DuplicateField);
        }
        let value = rest.strip_suffix(')').ok_or(DecodeError::InvalidValue)?;
        snapshot.readings[index] = Some(if let Some(unit) = FIELDS[index].wire_unit {
            let (number, actual_unit) = value.split_once('*').ok_or(DecodeError::InvalidValue)?;
            // Meter vendors differ in capitalization of var/kvarh.
            if !actual_unit.eq_ignore_ascii_case(unit) {
                return Err(DecodeError::InvalidValue);
            }
            Value::Number(Decimal::parse(number).map_err(|_| DecodeError::InvalidValue)?)
        } else {
            Value::Timestamp(parse_timestamp(value)?)
        });
    }
    if snapshot.readings.iter().all(Option::is_none) {
        return Err(DecodeError::NoReadings);
    }
    Ok(snapshot)
}

fn parse_timestamp(value: &str) -> Result<[u8; 13], DecodeError> {
    let bytes: [u8; 13] = value
        .as_bytes()
        .try_into()
        .map_err(|_| DecodeError::InvalidValue)?;
    if !bytes[..12].iter().all(u8::is_ascii_digit) || !matches!(bytes[12], b'S' | b'W') {
        return Err(DecodeError::InvalidValue);
    }
    let pair = |i| (bytes[i] - b'0') * 10 + bytes[i + 1] - b'0';
    let year = pair(0);
    let month = pair(2);
    let days = match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 => 29,
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return Err(DecodeError::InvalidValue),
    };
    if pair(4) == 0 || pair(4) > days || pair(6) > 23 || pair(8) > 59 || pair(10) > 59 {
        return Err(DecodeError::InvalidValue);
    }
    Ok(bytes)
}

/// Byte-stream decoder; a new '/' resynchronizes after a truncated or corrupt frame.
pub struct Decoder {
    bytes: [u8; MAX_TELEGRAM_LEN],
    len: usize,
    collecting: bool,
    ending: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            bytes: [0; MAX_TELEGRAM_LEN],
            len: 0,
            collecting: false,
            ending: false,
        }
    }

    pub fn reset(&mut self) {
        self.len = 0;
        self.collecting = false;
        self.ending = false;
    }

    pub fn push(&mut self, byte: u8) -> Option<Result<Snapshot, DecodeError>> {
        if byte == b'/' {
            self.reset();
            self.collecting = true;
        }
        if !self.collecting {
            return None;
        }
        if self.len == self.bytes.len() {
            self.reset();
            return Some(Err(DecodeError::Overflow));
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        if byte == b'!' {
            self.ending = true;
        }
        if self.ending && byte == b'\n' {
            self.collecting = false;
            return Some(parse_telegram(&self.bytes[..self.len]));
        }
        None
    }
}
