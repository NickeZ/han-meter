//! UART runs in interrupt context; MQTT/networking preempt all optional work.
use crate::{network, settings};
use core::{ffi::c_void, ptr};
use embassy_executor::SendSpawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use esp_hal::peripherals::WIFI;
use esp_rtos::embassy::Executor;
use static_cell::StaticCell;

// The main thread retains its larger stack for HTTP and synchronous TLS calls.
// This separate stack is exclusively for MQTT/networking, including interrupts.
const STACK_BYTES: usize = 32 * 1024;
pub(crate) const MQTT_PRIORITY: u32 = 1;
pub(crate) const BACKGROUND_PRIORITY: usize = 0;
const _: () = assert!(MQTT_PRIORITY as usize > BACKGROUND_PRIORITY);

struct Start {
    wifi: WIFI<'static>,
    control: &'static settings::SettingsControl,
    background: SendSpawner,
}
static START: Signal<CriticalSectionRawMutex, Start> = Signal::new();

pub(crate) fn start(
    wifi: WIFI<'static>,
    control: &'static settings::SettingsControl,
    background: SendSpawner,
) {
    START.signal(Start {
        wifi,
        control,
        background,
    });
    // Reserve the contiguous stack before Wi-Fi/TLS allocate their buffers.
    // SAFETY: no raw parameter is passed or dereferenced. All task arguments
    // cross through a synchronized signal whose contents must be Send. The
    // non-Send network stack is constructed and used only on this thread.
    unsafe {
        esp_radio_rtos_driver::task_create(
            "mqtt-network",
            entry,
            ptr::null_mut(),
            MQTT_PRIORITY,
            None,
            STACK_BYTES,
        );
    }
}

extern "C" fn entry(_: *mut c_void) {
    static EXECUTOR: StaticCell<Executor> = StaticCell::new();
    EXECUTOR.init_with(Executor::new).run(|spawner| {
        spawner.spawn(run(spawner).unwrap());
    });
}

#[embassy_executor::task]
async fn run(spawner: embassy_executor::Spawner) {
    let Start {
        wifi,
        control,
        background,
    } = START.wait().await;
    network::start(spawner, background, wifi, control).await;
}
