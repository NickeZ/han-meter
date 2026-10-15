use core::{cell::RefCell, fmt};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::Instant;
use esp_hal::uart::RxError;
use han_config::diagnostics::{DmaStats, Log, Snapshot, UartErrors};

static LOG: Log<CriticalSectionRawMutex> = Log::new();
static RX: Mutex<CriticalSectionRawMutex, RefCell<RxStats>> = Mutex::new(RefCell::new(RxStats {
    boot_id: 0,
    errors: UartErrors {
        overflow: 0,
        framing: 0,
        glitch: 0,
        parity: 0,
        other: 0,
    },
    dma: DmaStats {
        received_bytes: 0,
        errors: 0,
        overruns: 0,
    },
    last_valid_ms: None,
    last_interval_ms: None,
    max_interval_ms: None,
}));

struct RxStats {
    boot_id: u32,
    errors: UartErrors,
    dma: DmaStats,
    last_valid_ms: Option<u64>,
    last_interval_ms: Option<u64>,
    max_interval_ms: Option<u64>,
}

pub(crate) fn initialize(boot_id: u32) {
    RX.lock(|state| state.borrow_mut().boot_id = boot_id);
    record(format_args!(
        "Boot; debug log is in RAM and resets on restart"
    ));
}

pub(crate) fn record(args: fmt::Arguments<'_>) {
    LOG.record(Instant::now().as_millis(), args);
}

// Mirror existing infrequent USB messages. Receive errors use record() only,
// avoiding synchronous USB output in the UART task.
macro_rules! log {
    ($($arg:tt)*) => {{
        $crate::diagnostics::record(core::format_args!($($arg)*));
        esp_println::println!($($arg)*);
    }};
}
pub(crate) use log;

pub(crate) fn valid_telegram(now_ms: u64) {
    let interval = RX.lock(|state| {
        let mut state = state.borrow_mut();
        let interval = state.last_valid_ms.map(|last| now_ms.saturating_sub(last));
        state.last_valid_ms = Some(now_ms);
        state.last_interval_ms = interval;
        if let Some(interval) = interval {
            state.max_interval_ms = Some(state.max_interval_ms.unwrap_or(0).max(interval));
        }
        interval
    });
    match interval {
        None => record(format_args!("First valid telegram received")),
        Some(ms) if ms >= 1500 => record(format_args!("Gap between valid telegrams: {} ms", ms)),
        _ => {}
    }
}

/// Return a category index for rate limiting and its cumulative error count.
pub(crate) fn uart_error(error: RxError) -> (usize, u64) {
    RX.lock(|state| {
        let mut state = state.borrow_mut();
        let (index, count) = match error {
            RxError::FifoOverflowed => (0, &mut state.errors.overflow),
            RxError::FrameFormatViolated => (1, &mut state.errors.framing),
            RxError::GlitchOccurred => (2, &mut state.errors.glitch),
            RxError::ParityMismatch => (3, &mut state.errors.parity),
            _ => (4, &mut state.errors.other),
        };
        *count = count.saturating_add(1);
        (index, *count)
    })
}

pub(crate) fn dma_received(bytes: usize) {
    RX.lock(|state| {
        let mut state = state.borrow_mut();
        state.dma.received_bytes = state.dma.received_bytes.saturating_add(bytes as u64);
    });
}

pub(crate) fn dma_error(overrun: bool) -> u64 {
    RX.lock(|state| {
        let mut state = state.borrow_mut();
        state.dma.errors = state.dma.errors.saturating_add(1);
        if overrun {
            state.dma.overruns = state.dma.overruns.saturating_add(1);
        }
        state.dma.errors
    })
}

pub(crate) fn snapshot(after: u64) -> Snapshot {
    let now_ms = Instant::now().as_millis();
    let telemetry = crate::telemetry();
    let (boot_id, uart_error_types, dma, last_interval_ms, max_interval_ms) = RX.lock(|state| {
        let state = state.borrow();
        (
            state.boot_id,
            state.errors,
            state.dma,
            state.last_interval_ms,
            state.max_interval_ms,
        )
    });
    Snapshot {
        boot_id,
        uptime_ms: now_ms,
        valid_frames: telemetry.valid_frames,
        invalid_frames: telemetry.invalid_frames,
        uart_errors: telemetry.uart_errors,
        uart_error_types,
        dma,
        last_valid_age_ms: telemetry
            .last_valid_ms
            .map(|last| now_ms.saturating_sub(last)),
        last_interval_ms,
        max_interval_ms,
        log: LOG.page(after),
    }
}
