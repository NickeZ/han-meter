//! UART1 -> UHCI0 -> GDMA channel 0. All three peripherals belong exclusively
//! to this receiver. DMA runs continuously; the task polls completed chunks.
use crate::dma_ring::{self, CHUNK_SIZE, Ring, Storage};
use core::{
    mem::{align_of, offset_of, size_of},
    sync::atomic::{Ordering, fence},
};
use esp_hal::{
    Blocking,
    dma::{DmaDescriptor, DmaRxBuffer, Preparation},
    gpio::interconnect::InputSignal,
    peripherals::{DMA, DMA_CH0, GPIO0, UART1, UHCI0},
    uart::{
        Config, DataBits, Parity, RxConfig, RxError, StopBits, Uart,
        uhci::{self, Uhci, UhciDmaRxTransfer, UhciTx},
    },
};
use static_cell::StaticCell;

pub const FIFO_THRESHOLD: u16 = 32;

// This adapter is specific to the ESP32-C3 and esp-hal's GDMA descriptor ABI.
const _: () = {
    assert!(size_of::<dma_ring::Descriptor>() == size_of::<DmaDescriptor>());
    assert!(align_of::<dma_ring::Descriptor>() == align_of::<DmaDescriptor>());
    assert!(offset_of!(dma_ring::Descriptor, flags) == offset_of!(DmaDescriptor, flags));
    assert!(offset_of!(dma_ring::Descriptor, buffer) == offset_of!(DmaDescriptor, buffer));
    assert!(offset_of!(dma_ring::Descriptor, next) == offset_of!(DmaDescriptor, next));
};

struct Buffer(Ring);
pub struct ActiveBuffer(Ring);

// SAFETY: Storage is unique static internal RAM, aligned to 4 bytes. The ring
// has eight fixed, valid, circular descriptors and 256-byte chunks. RX owner
// checking prevents DMA from overwriting a chunk being copied by the CPU.
// ActiveBuffer exposes no storage references or operations that reset the ring.
// The transfer owns Buffer until the channel has stopped; forgetting it cannot
// free or reuse its static memory. This module never calls the HAL's wait()
// error path: cancellation plus an explicit parked-channel check reclaims it.
unsafe impl DmaRxBuffer for Buffer {
    type View = ActiveBuffer;
    type Final = Self;

    fn prepare(&mut self) -> Preparation {
        Preparation {
            // SAFETY: initial startup or restart after Receiver::restart has
            // confirmed the DMA channel is parked. No active view exists.
            start: unsafe { self.0.prepare() }.cast(),
            burst_transfer: Default::default(),
            check_owner: Some(true),
            auto_write_back: true,
        }
    }

    fn into_view(self) -> Self::View {
        ActiveBuffer(self.0)
    }
    fn from_view(view: Self::View) -> Self {
        Self(view.0)
    }
}

type Transfer = UhciDmaRxTransfer<'static, Blocking, Buffer>;

#[derive(Debug)]
pub enum DmaError {
    BufferExhausted,
    Descriptor,
    DmaFifo,
    DmaEof,
    InvalidLength,
}

#[derive(Debug)]
pub struct Error {
    pub uart: Option<RxError>,
    pub dma: Option<DmaError>,
}

pub struct Receiver {
    transfer: Option<Transfer>,
    // Retain the unused TX half so peripheral lifetime/clock ownership is clear.
    _tx: UhciTx<'static, Blocking>,
}

fn uart_config() -> Config {
    Config::default()
        .with_baudrate(115_200)
        .with_data_bits(DataBits::_8)
        .with_parity(Parity::None)
        .with_stop_bits(StopBits::_1)
        .with_rx(RxConfig::default().with_fifo_full_threshold(FIFO_THRESHOLD))
}

impl Receiver {
    pub fn new(
        uart: UART1<'static>,
        pin: GPIO0<'static>,
        uhci: UHCI0<'static>,
        channel: DMA_CH0<'static>,
    ) -> Self {
        let uart = Uart::new(uart, uart_config())
            .expect("UART configuration")
            .with_rx(InputSignal::from(pin).with_input_inverter(true));
        let mut uhci = Uhci::new(uart, uhci, channel);
        uhci.apply_rx_config(&uhci::RxConfig::default().with_chunk_limit(CHUNK_SIZE as u16))
            .expect("UHCI RX configuration");
        // Despite its HAL name, TxConfig sets UHCI's receive EOF controls.
        // Length EOF bounds each descriptor; idle EOF publishes the short tail
        // immediately, without waiting for the next meter telegram.
        uhci.apply_tx_config(
            &uhci::TxConfig::default()
                .with_idle_eof(true)
                .with_len_eof(true),
        )
        .expect("UHCI EOF configuration");
        let (rx, tx) = uhci.split();
        static STORAGE: StaticCell<Storage> = StaticCell::new();
        let buffer = Buffer(Ring::new(STORAGE.init(Storage::new())));
        let transfer = rx
            .read(buffer)
            .unwrap_or_else(|(error, _, _)| panic!("DMA start: {error:?}"));
        Self {
            transfer: Some(transfer),
            _tx: tx,
        }
    }

    fn error(&self) -> Option<Error> {
        // Read sticky raw flags without clearing them or touching the RX FIFO
        // while DMA is running. No UART/DMA interrupt handler is installed.
        let uart = UART1::regs().int_raw().read();
        let uart_error = if uart.rxfifo_ovf().bit_is_set() {
            Some(RxError::FifoOverflowed)
        } else if uart.frm_err().bit_is_set() {
            Some(RxError::FrameFormatViolated)
        } else if uart.glitch_det().bit_is_set() {
            Some(RxError::GlitchOccurred)
        } else if uart.parity_err().bit_is_set() {
            Some(RxError::ParityMismatch)
        } else {
            None
        };
        let dma = DMA::regs().int_ch(0).raw().read();
        let dma_error = if dma.in_dscr_empty().bit_is_set() || dma.in_dscr_err().bit_is_set() {
            let transfer = self.transfer.as_ref().unwrap();
            // SAFETY: the active ring has owner checking/writeback enabled.
            Some(if unsafe { transfer.0.is_full() } {
                DmaError::BufferExhausted
            } else {
                DmaError::Descriptor
            })
        } else if dma.infifo_ovf().bit_is_set() || dma.infifo_udf().bit_is_set() {
            Some(DmaError::DmaFifo)
        } else if dma.in_err_eof().bit_is_set() {
            Some(DmaError::DmaEof)
        } else {
            None
        };
        // A stalled consumer may exhaust DMA and then overflow UART too.
        // Preserve both diagnoses before recovery clears the hardware flags.
        (uart_error.is_some() || dma_error.is_some()).then_some(Error {
            uart: uart_error,
            dma: dma_error,
        })
    }

    pub fn read_chunk(&mut self, out: &mut [u8; CHUNK_SIZE]) -> Result<Option<usize>, Error> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        // SAFETY: owner checking and RX writeback are enabled. This task is the
        // sole CPU consumer; the ring returns a copy before releasing the chunk.
        let result =
            unsafe { self.transfer.as_mut().unwrap().0.read_chunk(out) }.map_err(|_| Error {
                uart: None,
                dma: Some(DmaError::InvalidLength),
            })?;
        // A fault during the copy invalidates the chunk before the parser sees it.
        if let Some(error) = self.error() {
            return Err(error);
        }
        Ok(result)
    }

    pub fn restart(&mut self) {
        let (mut rx, buffer) = self.transfer.take().unwrap().cancel();
        // cancel requests a stop. Do not rewrite descriptors until hardware has
        // acknowledged it. This is bounded to 1 ms; failure cannot reuse live RAM.
        let start = esp_hal::time::Instant::now();
        while !DMA::regs()
            .ch(0)
            .in_link()
            .read()
            .inlink_park()
            .bit_is_set()
        {
            assert!(start.elapsed().as_micros() < 1000, "DMA failed to stop");
        }
        fence(Ordering::Acquire);
        let _ = rx.uart_rx.check_for_errors();
        rx.uart_rx
            .apply_config(&uart_config())
            .expect("UART RX reset");
        // No transmitter is active. Reset UHCI's packet state along with DMA
        // so a partial pre-error chunk cannot be joined to fresh data.
        UHCI0::regs()
            .conf0()
            .modify(|_, w| w.rx_rst().set_bit().tx_rst().set_bit());
        UHCI0::regs()
            .conf0()
            .modify(|_, w| w.rx_rst().clear_bit().tx_rst().clear_bit());
        self.transfer = Some(
            rx.read(buffer)
                .unwrap_or_else(|(error, _, _)| panic!("DMA restart: {error:?}")),
        );
    }
}
