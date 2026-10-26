#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]

extern crate alloc;

mod broker;
mod diagnostics;
mod dma_ring;
mod log_shipping;
mod network;
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
use esp_hal::{clock::CpuClock, rng::Rng, timer::timg::TimerGroup};
use han_config::diagnostics::RateLimit;
use han_core::{Decoder, Telemetry, discovery};
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
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    diagnostics::initialize(Rng::new().random());
    spawner.spawn(status_led::run(peripherals.RMT, peripherals.GPIO10).unwrap());
    let (mqtt_settings, settings_store) =
        settings::initialize(esp_storage::FlashStorage::new(peripherals.FLASH));

    let rx = uart_dma::Receiver::new(
        peripherals.UART1,
        peripherals.GPIO0,
        peripherals.UHCI0,
        peripherals.DMA_CH0,
    );
    spawner.spawn(receive(rx).unwrap());
    spawner.spawn(report_diagnostics().unwrap());

    let controller = esp_radio::wifi::WifiController::new(peripherals.WIFI, Default::default())
        .expect("Wi-Fi initialization");
    let station = esp_radio::wifi::Interface::station();
    let id = discovery::device_id(station.mac_address());
    // DHCP hostnames use hyphens; keep the MQTT/Home Assistant identifier unchanged.
    let hostname = id.replace('_', "-");
    let mut dhcp = embassy_net::DhcpConfig::default();
    dhcp.hostname = Some(hostname.as_str().try_into().expect("DHCP hostname length"));
    let mut net_config = embassy_net::Config::dhcpv4(dhcp);
    net_config.ipv6 = embassy_net::ConfigV6::Slaac;
    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    // DHCP, DNS, HTTP, MQTT, log upload, and SNTP can coexist.
    static RESOURCES: StaticCell<embassy_net::StackResources<6>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        station,
        net_config,
        RESOURCES.init(embassy_net::StackResources::new()),
        seed,
    );
    spawner.spawn(network::runner(runner).unwrap());
    spawner.spawn(network::wifi(controller).unwrap());
    spawner.spawn(settings::http(stack, mqtt_settings, settings_store, hostname.clone()).unwrap());
    spawner.spawn(settings::address_log(stack).unwrap());
    if let Some(config) = config::LOGGING {
        spawner.spawn(log_shipping::run(stack, config, hostname.clone()).unwrap());
    }
    log!(
        "han-meter {} device={} hostname={} RX=GPIO0 inverted 115200 8N1",
        env!("CARGO_PKG_VERSION"),
        id,
        hostname
    );
    network::run(stack, &id, mqtt_settings).await
}

#[embassy_executor::task]
async fn receive(mut rx: uart_dma::Receiver) {
    // Keep the 4 KiB frame buffer out of the task's stack.
    static DECODER: StaticCell<Decoder> = StaticCell::new();
    let decoder = DECODER.init(Decoder::new());
    let mut bytes = [0; dma_ring::CHUNK_SIZE];
    let mut uart_logs: [RateLimit; 5] = core::array::from_fn(|_| RateLimit::default());
    let mut dma_logs = RateLimit::default();
    let mut invalid_logs = RateLimit::default();
    diagnostics::record(format_args!(
        "UART DMA active: UHCI0/GDMA0, {} bytes, {} x {} chunks; RX FIFO threshold={}",
        dma_ring::CAPACITY,
        dma_ring::CHUNKS,
        dma_ring::CHUNK_SIZE,
        uart_dma::FIFO_THRESHOLD
    ));
    loop {
        // Bound work per poll even if the input is continuous. DMA keeps filling
        // the ring while parsing and while the executor runs network tasks.
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
                                "UART {:?}; count={}; partial telegram discarded; DMA restarted",
                                error, count
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
                                "DMA {:?}; count={}; buffered data discarded; reception restarted",
                                error, count
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
    }
}
