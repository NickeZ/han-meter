//! Minimal SNTP packet validation. The transport must check source address/port.
pub fn request(nonce: [u8; 8]) -> [u8; 48] {
    let mut packet = [0; 48];
    packet[0] = 0x23; // version 4, client
    packet[40..48].copy_from_slice(&nonce);
    packet
}

pub fn unix_ms(response: &[u8], nonce: [u8; 8]) -> Option<u64> {
    if response.len() < 48
        || response[0] >> 6 == 3
        || !matches!((response[0] >> 3) & 7, 3 | 4)
        || response[0] & 7 != 4
        || !(1..=15).contains(&response[1])
        || response[24..32] != nonce
        || response[40..48] == [0; 8]
    {
        return None;
    }
    let seconds = u32::from_be_bytes(response[40..44].try_into().ok()?) as u64;
    let fraction = u32::from_be_bytes(response[44..48].try_into().ok()?) as u64;
    // NTP era 0 until Feb 2036, era 1 afterwards (valid through 2106).
    let seconds = if seconds < 2_208_988_800 {
        seconds + (1u64 << 32)
    } else {
        seconds
    } - 2_208_988_800;
    (seconds >= 1_577_836_800).then_some(seconds * 1000 + ((fraction * 1000) >> 32))
}
