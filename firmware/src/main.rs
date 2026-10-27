#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]

extern crate alloc;

mod bootloader;
mod broker;
mod diagnostics;
mod dma_ring;
mod http_socket;
mod http_transport;
mod log_shipping;
mod log_transport;
mod network;
mod scheduling;
mod settings;
mod status_led;
mod uart_dma;
mod config {
    include!(concat!(env!("OUT_DIR"), "/config.rs"));
}

use crate::diagnostics::log;
use core::cell::RefCell;
use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    interrupt::Priority,
    peripherals::{DMA_CH0, GPIO0, UART1, UHCI0},
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_rtos::embassy::InterruptExecutor;
use han_config::diagnostics::RateLimit;
use han_core::{Decoder, Telemetry};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

pub(crate) static TELEMETRY: Mutex<CriticalSectionRawMutex, RefCell<Telemetry>> =
    Mutex::new(RefCell::new(Telemetry::new()));
pub(crate) static UPDATED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub(crate) fn telemetry() -> Telemetry {
    TELEMETRY.lock(|t| *t.borrow())
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_80MHz));
    esp_alloc::heap_allocator!(size: 96 * 1024);
    // The ESP-IDF bootloader has finished using this separate RAM region.
    // TLS shares the heap with Wi-Fi/MQTT; grow it without taking main-stack RAM.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    // All optional application work stays here, below the MQTT/network thread.
    esp_rtos::CurrentThreadHandle::get().set_priority(scheduling::BACKGROUND_PRIORITY);
    diagnostics::initialize(Rng::new().random());
    diagnostics::log_heap("startup");
    spawner.spawn(diagnostics::usb(peripherals.USB_DEVICE).unwrap());
    spawner.spawn(status_led::run(peripherals.RMT, peripherals.GPIO10).unwrap());
    // UART reception preempts MQTT/networking and all background work.
    // Priority 1 preempts thread mode without preempting the RTOS timer ISR.
    // FROM_CPU_INTR0 is already reserved by esp_rtos::start.
    static RX_EXECUTOR: StaticCell<InterruptExecutor<1>> = StaticCell::new();
    let rx_executor = RX_EXECUTOR.init(InterruptExecutor::new(peripherals.FROM_CPU_INTR1));
    rx_executor.start(Priority::Priority1).spawn(
        receive(
            peripherals.UART1,
            peripherals.GPIO0,
            peripherals.UHCI0,
            peripherals.DMA_CH0,
        )
        .unwrap(),
    );
    spawner.spawn(report_diagnostics().unwrap());

    let (mqtt_settings, settings_store) =
        settings::initialize(esp_storage::FlashStorage::new(peripherals.FLASH));
    spawner.spawn(settings::http(mqtt_settings, settings_store).unwrap());
    scheduling::start(peripherals.WIFI, mqtt_settings, spawner.make_send());
    log!(
        "Task priorities: UART interrupt, MQTT/network={}, background={}",
        scheduling::MQTT_PRIORITY,
        scheduling::BACKGROUND_PRIORITY
    );
    core::future::pending().await
}

#[embassy_executor::task]
async fn receive(
    uart: UART1<'static>,
    pin: GPIO0<'static>,
    uhci: UHCI0<'static>,
    channel: DMA_CH0<'static>,
) {
    // Construct the receiver on its owning executor: no active DMA buffer or
    // raw ring pointers need to be sent across execution contexts.
    let mut rx = uart_dma::Receiver::new(uart, pin, uhci, channel);
    // Keep the 4 KiB frame buffer out of the task's stack.
    static DECODER: StaticCell<Decoder> = StaticCell::new();
    let decoder = DECODER.init_with(Decoder::new);
    let mut bytes = [0; dma_ring::CHUNK_SIZE];
    let mut uart_logs: [RateLimit; 5] = core::array::from_fn(|_| RateLimit::default());
    let mut dma_logs = RateLimit::default();
    let mut invalid_logs = RateLimit::default();
    let mut poll_delay_logs = RateLimit::default();
    diagnostics::record(format_args!(
        "UART DMA active: UHCI0/GDMA0, {} bytes, {} x {} chunks; RX FIFO threshold={}; interrupt executor, poll=5 ms",
        dma_ring::CAPACITY,
        dma_ring::CHUNKS,
        dma_ring::CHUNK_SIZE,
        uart_dma::FIFO_THRESHOLD
    ));
    let mut last_poll_ms = Instant::now().as_millis();
    loop {
        let now_ms = Instant::now().as_millis();
        let poll_gap_ms = now_ms.saturating_sub(last_poll_ms);
        last_poll_ms = now_ms;
        // Ten missed polling intervals warrant a diagnostic before the 2 KiB
        // ring fills. Interrupt masking can still delay this executor.
        if poll_gap_ms >= 50 && poll_delay_logs.admit(now_ms) {
            diagnostics::record(format_args!("UART DMA poll delayed: {} ms", poll_gap_ms));
        }
        // Bound interrupt work even if the input is continuous. This path must
        // remain allocation-free and must not write to USB or wait for networking.
        for _ in 0..dma_ring::CHUNKS {
            match rx.read_chunk(&mut bytes) {
                Ok(None) => break,
                Ok(Some(len)) => {
                    diagnostics::dma_received(len);
                    for &byte in &bytes[..len] {
                        if let Some(result) = decoder.push(byte) {
                            let now_ms = Instant::now().as_millis();
                            match result {
                                Ok(snapshot) => {
                                    TELEMETRY
                                        .lock(|state| state.borrow_mut().update(snapshot, now_ms));
                                    diagnostics::valid_telegram(now_ms);
                                }
                                Err(error) => {
                                    TELEMETRY.lock(|state| {
                                        let mut state = state.borrow_mut();
                                        state.invalid_frames =
                                            state.invalid_frames.saturating_add(1)
                                    });
                                    if invalid_logs.admit(now_ms) {
                                        diagnostics::record(format_args!(
                                            "Telegram rejected: {:?}",
                                            error
                                        ));
                                    }
                                }
                            }
                            UPDATED.signal(());
                        }
                    }
                }
                Err(error) => {
                    decoder.reset();
                    rx.restart();
                    if let Some(error) = error.uart {
                        TELEMETRY.lock(|state| {
                            let mut state = state.borrow_mut();
                            state.uart_errors = state.uart_errors.saturating_add(1);
                        });
                        let (kind, count) = diagnostics::uart_error(error);
                        if uart_logs[kind].admit(Instant::now().as_millis()) {
                            diagnostics::record(format_args!(
                                "UART {:?}; count={}; poll_gap={} ms; partial telegram discarded; DMA restarted",
                                error, count, poll_gap_ms
                            ));
                        }
                    }
                    if let Some(error) = error.dma {
                        let count = diagnostics::dma_error(matches!(
                            error,
                            uart_dma::DmaError::BufferExhausted
                        ));
                        if dma_logs.admit(Instant::now().as_millis()) {
                            diagnostics::record(format_args!(
                                "DMA {:?}; count={}; poll_gap={} ms; buffered data discarded; reception restarted",
                                error, count, poll_gap_ms
                            ));
                        }
                    }
                    break;
                }
            }
        }
        Timer::after(Duration::from_millis(5)).await;
    }
}

#[embassy_executor::task]
async fn report_diagnostics() {
    loop {
        Timer::after(Duration::from_secs(60)).await;
        let state = telemetry();
        log!(
            "uptime={}s valid={} invalid={} uart_errors={} meter_fresh={}",
            Instant::now().as_secs(),
            state.valid_frames,
            state.invalid_frames,
            state.uart_errors,
            state.online(Instant::now().as_millis())
        );
        diagnostics::log_heap("periodic");
    }
}
