//! Two-sector journal. The inactive sector is erased; the commit word is written last.
use crate::MqttSettings;
use embedded_storage::nor_flash::NorFlash;

pub const SECTOR_SIZE: u32 = 4096;
pub const STORAGE_SIZE: u32 = 2 * SECTOR_SIZE;
const MAGIC: &[u8; 8] = b"HANMQTT\0";
const VERSION: u32 = 1;
const CHECKSUM: usize = 668;
const COMMIT: usize = 672;
const RECORD_SIZE: usize = 676;

#[repr(align(4))]
struct Record([u8; RECORD_SIZE]);
#[repr(align(4))]
struct Block([u8; 256]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Region,
    ForeignData,
    Io,
    Verification,
    InvalidSettings,
    SequenceExhausted,
}

pub struct Store<F> {
    flash: F,
    offset: u32,
    active: Option<(usize, u64, MqttSettings)>,
    // Any ambiguous write/read failure requires reopening before another erase.
    healthy: bool,
}

impl<F: NorFlash> Store<F> {
    pub fn open(mut flash: F, offset: u32) -> Result<Self, Error> {
        if F::ERASE_SIZE != SECTOR_SIZE as usize
            || F::WRITE_SIZE != 4
            || !4usize.is_multiple_of(F::READ_SIZE)
            || !offset.is_multiple_of(SECTOR_SIZE)
            || offset
                .checked_add(STORAGE_SIZE)
                .is_none_or(|end| end as usize > flash.capacity())
        {
            return Err(Error::Region);
        }
        let mut active: Option<(usize, u64, MqttSettings)> = None;
        let mut foreign = false;
        for slot in 0..2 {
            let mut record = Record([0xff; RECORD_SIZE]);
            flash
                .read(offset + slot as u32 * SECTOR_SIZE, &mut record.0)
                .map_err(|_| Error::Io)?;
            if let Some((sequence, settings)) = decode(&record.0) {
                if active
                    .as_ref()
                    .is_none_or(|(_, previous, _)| sequence > *previous)
                {
                    active = Some((slot, sequence, settings));
                }
            } else if record.0[..8] == MAGIC[..] {
                // Unknown versions belong to newer firmware; never overwrite them.
                if record.0[8..12] != VERSION.to_le_bytes()
                    && &record.0[COMMIT..] == b"DONE"
                    && record.0[CHECKSUM..COMMIT] == crc32(&record.0[..CHECKSUM]).to_le_bytes()
                {
                    return Err(Error::ForeignData);
                }
            } else {
                // Read the complete sector before considering it erased. A partial first
                // write of our magic is also recoverable after loss of power.
                let partial_magic = record.0[..8]
                    .iter()
                    .zip(MAGIC)
                    .all(|(&a, &b)| a == b || a == 0xff)
                    && record.0[8..].iter().all(|&b| b == 0xff);
                let mut blank_tail = true;
                let mut block = Block([0; 256]);
                for position in (RECORD_SIZE as u32..SECTOR_SIZE).step_by(256) {
                    let len = (SECTOR_SIZE - position).min(256) as usize;
                    flash
                        .read(
                            offset + slot as u32 * SECTOR_SIZE + position,
                            &mut block.0[..len],
                        )
                        .map_err(|_| Error::Io)?;
                    blank_tail &= block.0[..len].iter().all(|&b| b == 0xff);
                }
                foreign |= !partial_magic || !blank_tail;
            }
        }
        // A valid slot establishes ownership of the pair. An interrupted erase
        // of the inactive slot can leave an arbitrary mixture of old bytes/FF.
        if foreign && active.is_none() {
            return Err(Error::ForeignData);
        }
        Ok(Self {
            flash,
            offset,
            active,
            healthy: true,
        })
    }

    pub fn load(&self) -> Option<&MqttSettings> {
        self.active.as_ref().map(|(_, _, settings)| settings)
    }

    pub fn is_available(&self) -> bool {
        self.healthy
    }

    pub fn save(&mut self, settings: &MqttSettings) -> Result<bool, Error> {
        settings.validate().map_err(|_| Error::InvalidSettings)?;
        if !self.healthy {
            return Err(Error::Io);
        }
        if self.load() == Some(settings) {
            return Ok(false);
        }
        let (slot, sequence) = match &self.active {
            Some((slot, seq, _)) => (
                1 - slot,
                seq.checked_add(1).ok_or(Error::SequenceExhausted)?,
            ),
            None => (0, 1),
        };
        let address = self.offset + slot as u32 * SECTOR_SIZE;
        let record = encode(settings, sequence);
        self.healthy = false;
        self.flash
            .erase(address, address + SECTOR_SIZE)
            .map_err(|_| Error::Io)?;
        self.flash
            .write(address, &record.0[..COMMIT])
            .map_err(|_| Error::Io)?;
        let mut readback = Record([0; RECORD_SIZE]);
        self.flash
            .read(address, &mut readback.0)
            .map_err(|_| Error::Io)?;
        if readback.0[..COMMIT] != record.0[..COMMIT] || readback.0[COMMIT..] != [0xff; 4] {
            return Err(Error::Verification);
        }
        self.flash
            .write(address + COMMIT as u32, &record.0[COMMIT..])
            .map_err(|_| Error::Io)?;
        self.flash
            .read(address, &mut readback.0)
            .map_err(|_| Error::Io)?;
        if readback.0 != record.0 {
            return Err(Error::Verification);
        }
        self.active = Some((slot, sequence, settings.clone()));
        self.healthy = true;
        Ok(true)
    }

    pub fn into_inner(self) -> F {
        self.flash
    }
}

fn encode(settings: &MqttSettings, sequence: u64) -> Record {
    let mut bytes = Record([0xff; RECORD_SIZE]);
    bytes.0[..8].copy_from_slice(MAGIC);
    bytes.0[8..12].copy_from_slice(&VERSION.to_le_bytes());
    bytes.0[12..20].copy_from_slice(&sequence.to_le_bytes());
    bytes.0[26..28].copy_from_slice(&settings.port.to_le_bytes());
    let mut position = 28;
    for (index, value) in [
        settings.host.as_str(),
        settings.username.as_str(),
        settings.password.as_str(),
    ]
    .into_iter()
    .enumerate()
    {
        bytes.0[20 + index * 2..22 + index * 2]
            .copy_from_slice(&(value.len() as u16).to_le_bytes());
        bytes.0[position..position + value.len()].copy_from_slice(value.as_bytes());
        position += value.len();
    }
    let crc = crc32(&bytes.0[..CHECKSUM]);
    bytes.0[CHECKSUM..COMMIT].copy_from_slice(&crc.to_le_bytes());
    bytes.0[COMMIT..].copy_from_slice(b"DONE");
    bytes
}

fn decode(bytes: &[u8; RECORD_SIZE]) -> Option<(u64, MqttSettings)> {
    if &bytes[..8] != MAGIC
        || bytes[8..12] != VERSION.to_le_bytes()
        || &bytes[COMMIT..] != b"DONE"
        || bytes[CHECKSUM..COMMIT] != crc32(&bytes[..CHECKSUM]).to_le_bytes()
    {
        return None;
    }
    let sequence = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
    let port = u16::from_le_bytes(bytes[26..28].try_into().ok()?);
    let mut values = [""; 3];
    let mut position = 28;
    for (index, value) in values.iter_mut().enumerate() {
        let len =
            u16::from_le_bytes(bytes[20 + index * 2..22 + index * 2].try_into().ok()?) as usize;
        let end = position + len;
        if end > CHECKSUM {
            return None;
        }
        *value = core::str::from_utf8(&bytes[position..end]).ok()?;
        position = end;
    }
    Some((
        sequence,
        MqttSettings::new(values[0], port, values[1], values[2]).ok()?,
    ))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
