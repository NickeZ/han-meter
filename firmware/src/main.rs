#![no_std]
#![no_main]
#![deny(clippy::mem_forget)]

extern crate alloc;

mod network;
mod settings;
mod status_led;
mod config {
    include!(concat!(env!("OUT_DIR"), "/config.rs"));
}

use core::cell::RefCell;
use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::{
    Async,
    clock::CpuClock,
    gpio::interconnect::InputSignal,
    interrupt::software::SoftwareInterruptControl,
    rng::Rng,
    timer::timg::TimerGroup,
    uart::{Config, DataBits, Parity, StopBits, UartRx},
};
use esp_println::println;
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
    let interrupts = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, interrupts.software_interrupt0);
    spawner.spawn(status_led::run(peripherals.RMT, peripherals.GPIO10).unwrap());
    let (mqtt_settings, settings_store) =
        settings::initialize(esp_storage::FlashStorage::new(peripherals.FLASH));

    let rx = UartRx::new(
        peripherals.UART1,
        Config::default()
            .with_baudrate(115_200)
            .with_data_bits(DataBits::_8)
            .with_parity(Parity::None)
            .with_stop_bits(StopBits::_1),
    )
    .expect("UART configuration")
    .with_rx(InputSignal::from(peripherals.GPIO0).with_input_inverter(true))
    .into_async();
    spawner.spawn(receive(rx).unwrap());
    spawner.spawn(diagnostics().unwrap());

    let (controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).expect("Wi-Fi initialization");
    let id = discovery::device_id(interfaces.station.mac_address());
    // DHCP hostnames use hyphens; keep the MQTT/Home Assistant identifier unchanged.
    let hostname = id.replace('_', "-");
    let mut dhcp = embassy_net::DhcpConfig::default();
    dhcp.hostname = Some(hostname.as_str().try_into().expect("DHCP hostname length"));
    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    static RESOURCES: StaticCell<embassy_net::StackResources<4>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(dhcp),
        RESOURCES.init(embassy_net::StackResources::new()),
        seed,
    );
    spawner.spawn(network::runner(runner).unwrap());
    spawner.spawn(network::wifi(controller).unwrap());
    spawner.spawn(settings::http(stack, mqtt_settings, settings_store, hostname.clone()).unwrap());
    spawner.spawn(settings::address_log(stack).unwrap());
    println!(
        "han-meter {} device={} hostname={} RX=GPIO0 inverted 115200 8N1",
        env!("CARGO_PKG_VERSION"),
        id,
        hostname
    );
    network::run(stack, &id, mqtt_settings).await
}

#[embassy_executor::task]
async fn receive(mut rx: UartRx<'static, Async>) {
    // Keep the 4 KiB frame buffer out of the task's stack.
    static DECODER: StaticCell<Decoder> = StaticCell::new();
    let decoder = DECODER.init(Decoder::new());
    let mut bytes = [0; 128];
    loop {
        match rx.read_async(&mut bytes).await {
            Ok(len) => {
                for &byte in &bytes[..len] {
                    if let Some(result) = decoder.push(byte) {
                        TELEMETRY.lock(|state| {
                            let mut state = state.borrow_mut();
                            match result {
                                Ok(snapshot) => state.update(snapshot, Instant::now().as_millis()),
                                Err(_) => {
                                    state.invalid_frames = state.invalid_frames.saturating_add(1)
                                }
                            }
                        });
                        UPDATED.signal(());
                    }
                }
            }
            Err(_) => {
                decoder.reset();
                TELEMETRY.lock(|state| {
                    let mut state = state.borrow_mut();
                    state.uart_errors = state.uart_errors.saturating_add(1);
                });
                // The HAL clears reported error flags; drain any buffered tail before resync.
                let _ = rx.read_buffered(&mut bytes);
                Timer::after(Duration::from_millis(1)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn diagnostics() {
    loop {
        Timer::after(Duration::from_secs(60)).await;
        let state = telemetry();
        println!(
            "uptime={}s valid={} invalid={} uart_errors={} meter_fresh={}",
            Instant::now().as_secs(),
            state.valid_frames,
            state.invalid_frames,
            state.uart_errors,
            state.online(Instant::now().as_millis())
        );
    }
}
