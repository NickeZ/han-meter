//! Minimal SNTP packet validation. The transport must check source address/port.
pub mod client;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseError {
    ShortPacket,
    Version,
    Mode,
    Origin,
    KissOfDeath([u8; 4]),
    Unsynchronized,
    Stratum,
    ZeroTimestamp,
    DateBefore2020,
}

pub fn request(nonce: [u8; 8]) -> [u8; 48] {
    let mut packet = [0; 48];
    packet[0] = 0x23; // version 4, client
    packet[40..48].copy_from_slice(&nonce);
    packet
}

pub fn unix_ms(response: &[u8], nonce: [u8; 8]) -> Result<u64, ResponseError> {
    if response.len() < 48 {
        return Err(ResponseError::ShortPacket);
    }
    if !matches!((response[0] >> 3) & 7, 3 | 4) {
        return Err(ResponseError::Version);
    }
    if response[0] & 7 != 4 {
        return Err(ResponseError::Mode);
    }
    if response[24..32] != nonce {
        return Err(ResponseError::Origin);
    }
    if response[1] == 0 {
        return Err(ResponseError::KissOfDeath(
            response[12..16].try_into().unwrap(),
        ));
    }
    if response[0] >> 6 == 3 {
        return Err(ResponseError::Unsynchronized);
    }
    if response[1] > 15 {
        return Err(ResponseError::Stratum);
    }
    if response[40..48] == [0; 8] {
        return Err(ResponseError::ZeroTimestamp);
    }
    let seconds = u32::from_be_bytes(response[40..44].try_into().unwrap()) as u64;
    let fraction = u32::from_be_bytes(response[44..48].try_into().unwrap()) as u64;
    // NTP era 0 until Feb 2036, era 1 afterwards (valid through 2106).
    let seconds = if seconds < 2_208_988_800 {
        seconds + (1u64 << 32)
    } else {
        seconds
    } - 2_208_988_800;
    if seconds < 1_577_836_800 {
        return Err(ResponseError::DateBefore2020);
    }
    Ok(seconds * 1000 + ((fraction * 1000) >> 32))
}
