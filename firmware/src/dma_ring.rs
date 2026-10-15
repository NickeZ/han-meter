//! Fixed DMA receive ring. The device must check descriptor ownership and stop
//! on a CPU-owned descriptor. Completed data is copied before returning ownership.
//! Kept independent of the HAL so the actual ring logic can be tested on the host.
#![deny(unsafe_op_in_unsafe_fn)]

use core::{
    marker::PhantomData,
    ptr,
    sync::atomic::{Ordering, fence},
};

pub const CHUNK_SIZE: usize = 256;
pub const CHUNKS: usize = 8;
pub const CAPACITY: usize = CHUNK_SIZE * CHUNKS;
const DMA_OWNED: u32 = 1 << 31;
const _: () = {
    assert!(CHUNKS >= 2);
    assert!(CHUNK_SIZE > 0 && CHUNK_SIZE <= 4095);
    assert!(CHUNK_SIZE.is_multiple_of(4));
};

// ESP32-C3 GDMA descriptor layout. uart_dma checks this against the HAL's type.
#[repr(C)]
pub struct Descriptor {
    pub flags: u32,
    pub buffer: *mut u8,
    pub next: *mut Descriptor,
}

#[repr(C, align(4))]
pub struct Storage {
    descriptors: [Descriptor; CHUNKS],
    bytes: [u8; CAPACITY],
}

impl Storage {
    pub const fn new() -> Self {
        Self {
            descriptors: [const {
                Descriptor {
                    flags: 0,
                    buffer: ptr::null_mut(),
                    next: ptr::null_mut(),
                }
            }; CHUNKS],
            bytes: [0; CAPACITY],
        }
    }
}

pub struct Ring {
    descriptors: *mut Descriptor,
    bytes: *mut u8,
    next: usize,
    // Own the unique, permanent storage without keeping Rust references into
    // memory that DMA modifies. No Clone/Copy or shared access is provided.
    _storage: PhantomData<&'static mut Storage>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidLength;

impl Ring {
    pub fn new(storage: &'static mut Storage) -> Self {
        Self {
            descriptors: ptr::addr_of_mut!(storage.descriptors).cast(),
            bytes: ptr::addr_of_mut!(storage.bytes).cast(),
            next: 0,
            _storage: PhantomData,
        }
    }

    /// Prepare all descriptors, discarding any old data.
    ///
    /// # Safety
    /// DMA must be stopped, with no outstanding memory accesses, until the
    /// caller starts the next transfer. Storage must be in DMA-accessible RAM.
    pub unsafe fn prepare(&mut self) -> *mut Descriptor {
        for index in 0..CHUNKS {
            // SAFETY: these pointers stay inside our exclusively owned arrays;
            // the caller guarantees the device is not accessing them.
            unsafe {
                self.descriptors.add(index).write(Descriptor {
                    flags: DMA_OWNED | CHUNK_SIZE as u32,
                    buffer: self.bytes.add(index * CHUNK_SIZE),
                    next: self.descriptors.add((index + 1) % CHUNKS),
                });
            }
        }
        self.next = 0;
        fence(Ordering::Release);
        self.descriptors
    }

    /// Copy one completed descriptor, including a short or zero-length idle EOF.
    /// Returns None while the next descriptor still belongs to DMA.
    ///
    /// # Safety
    /// The transfer must use ownership checking and RX descriptor writeback.
    /// DMA must relinquish ownership only after writing the data and length.
    /// No other CPU code may access the ring's storage during this call.
    pub unsafe fn read_chunk(
        &mut self,
        out: &mut [u8; CHUNK_SIZE],
    ) -> Result<Option<usize>, InvalidLength> {
        // SAFETY: next is always a valid index. Volatile reads observe the
        // hardware-owned flags; no Rust reference aliases the descriptor.
        let flags_ptr = unsafe { ptr::addr_of_mut!((*self.descriptors.add(self.next)).flags) };
        let flags = unsafe { flags_ptr.read_volatile() };
        if flags & DMA_OWNED != 0 {
            return Ok(None);
        }
        fence(Ordering::Acquire);
        let len = ((flags >> 12) & 0xfff) as usize;
        if len > CHUNK_SIZE {
            return Err(InvalidLength);
        }
        // SAFETY: ownership checking prevents DMA from reusing this chunk.
        // The length is bounded and `out` cannot alias our exclusively owned
        // storage. Read the prefix even for a short final descriptor.
        unsafe {
            ptr::copy_nonoverlapping(
                self.bytes.add(self.next * CHUNK_SIZE),
                out.as_mut_ptr(),
                len,
            );
        }
        // Finish reading before publishing ownership in one aligned word write.
        // Buffer/next pointers are immutable throughout the active transfer.
        fence(Ordering::Release);
        unsafe { flags_ptr.write_volatile(DMA_OWNED | CHUNK_SIZE as u32) };
        self.next = (self.next + 1) % CHUNKS;
        Ok(Some(len))
    }

    /// # Safety
    /// Same active-transfer ownership contract as `read_chunk`.
    pub unsafe fn is_full(&self) -> bool {
        (0..CHUNKS).all(|index| unsafe {
            ptr::addr_of!((*self.descriptors.add(index)).flags).read_volatile() & DMA_OWNED == 0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Device {
        next: *mut Descriptor,
    }

    impl Device {
        // Model the hardware contract: publish completion after data, and stop
        // before overwriting a descriptor the consumer has not released.
        fn write(&mut self, data: &[u8]) -> bool {
            assert!(data.len() <= CHUNK_SIZE);
            unsafe {
                let descriptor = &mut *self.next;
                if descriptor.flags & DMA_OWNED == 0 {
                    return false;
                }
                ptr::copy_nonoverlapping(data.as_ptr(), descriptor.buffer, data.len());
                descriptor.flags = CHUNK_SIZE as u32 | ((data.len() as u32) << 12) | (1 << 30);
                self.next = descriptor.next;
            }
            true
        }
    }

    fn setup() -> (Ring, Device) {
        let mut ring = Ring::new(Box::leak(Box::new(Storage::new())));
        let next = unsafe { ring.prepare() };
        (ring, Device { next })
    }

    #[test]
    fn delayed_consumer_preserves_full_ring_and_wraps_without_overwrite() {
        let (mut ring, mut device) = setup();
        let mut out = [0; CHUNK_SIZE];
        assert_eq!(unsafe { ring.read_chunk(&mut out) }, Ok(None));
        for value in 0..CHUNKS {
            assert!(device.write(&[value as u8; CHUNK_SIZE]));
        }
        assert!(unsafe { ring.is_full() });
        assert!(!device.write(&[99; CHUNK_SIZE]));
        for value in 0..CHUNKS * 4 {
            assert_eq!(unsafe { ring.read_chunk(&mut out) }, Ok(Some(CHUNK_SIZE)));
            assert_eq!(out, [value as u8; CHUNK_SIZE]);
            assert!(device.write(&[(value + CHUNKS) as u8; CHUNK_SIZE]));
        }
    }

    #[test]
    fn short_and_empty_eofs_use_chunk_prefix_including_last_descriptor() {
        let (mut ring, mut device) = setup();
        let mut out = [0xa5; CHUNK_SIZE];
        for round in 0..4 {
            for index in 0..CHUNKS {
                let len = if round == 0 {
                    CHUNK_SIZE
                } else {
                    (index * 13 + round) % 100
                };
                let data = vec![(index + round) as u8; len];
                assert!(device.write(&data));
                assert_eq!(unsafe { ring.read_chunk(&mut out) }, Ok(Some(len)));
                assert_eq!(&out[..len], data);
            }
        }
        assert!(device.write(&[]));
        assert_eq!(unsafe { ring.read_chunk(&mut out) }, Ok(Some(0)));
        assert_eq!(unsafe { ring.read_chunk(&mut out) }, Ok(None));
    }

    #[test]
    fn invalid_length_is_rejected_and_stopped_ring_can_restart() {
        let (mut ring, mut device) = setup();
        let mut out = [0xa5; CHUNK_SIZE];
        unsafe { (*device.next).flags = ((CHUNK_SIZE as u32 + 1) << 12) | CHUNK_SIZE as u32 };
        assert_eq!(unsafe { ring.read_chunk(&mut out) }, Err(InvalidLength));
        assert_eq!(out, [0xa5; CHUNK_SIZE]);
        device.next = unsafe { ring.prepare() };
        assert_eq!(unsafe { ring.read_chunk(&mut out) }, Ok(None));
        assert!(device.write(b"/fresh telegram"));
        assert_eq!(unsafe { ring.read_chunk(&mut out) }, Ok(Some(15)));
        assert_eq!(&out[..15], b"/fresh telegram");
    }

    #[test]
    fn telegrams_decode_across_short_dma_chunks_and_ring_wraps() {
        let body = "/TEST5meter\r\n\r\n1-0:1.7.0(1.234*kW)\r\n!";
        let crc = crc::Crc::<u16>::new(&crc::CRC_16_ARC).checksum(body.as_bytes());
        let frame = format!("{body}{crc:04X}\r\n");
        let (mut ring, mut device) = setup();
        let mut decoder = han_core::Decoder::new();
        let mut out = [0; CHUNK_SIZE];
        let mut valid = 0;
        for _ in 0..20 {
            for bytes in frame.as_bytes().chunks(7) {
                assert!(device.write(bytes));
                let len = unsafe { ring.read_chunk(&mut out) }.unwrap().unwrap();
                for &byte in &out[..len] {
                    if let Some(result) = decoder.push(byte) {
                        assert!(result.is_ok());
                        valid += 1;
                    }
                }
            }
        }
        assert_eq!(valid, 20);
    }
}
