//! Waveshare ESP32-C3-Zero onboard WS2812: GPIO10, RGB byte order.
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::{
    gpio::Level,
    peripherals::{GPIO10, RMT},
    rmt::{PulseCode, Rmt, TxChannelConfig, TxChannelCreator},
    time::Rate,
};
use esp_println::println;

const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const GREEN_BRIGHTNESS: u8 = 16;
static WIFI_CONNECTED: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub(crate) fn set_wifi_connected(connected: bool) {
    WIFI_CONNECTED.signal(connected);
}

#[embassy_executor::task]
pub(crate) async fn run(peripheral: RMT<'static>, pin: GPIO10<'static>) {
    let rmt = Rmt::new(peripheral, Rate::from_mhz(80))
        .expect("Status LED RMT clock")
        .into_async();
    let mut channel = rmt
        .channel0
        .configure_tx(
            &TxChannelConfig::default()
                .with_clk_divider(1)
                .with_idle_output_level(Level::Low)
                .with_idle_output(true),
        )
        .expect("Status LED RMT channel")
        .with_pin(pin);

    // Start with a reset interval even if the LED retained a color across a CPU reset.
    Timer::after_micros(300).await;
    let mut connected = WIFI_CONNECTED.try_take().unwrap_or(false);
    let mut lit = false;
    loop {
        lit = !connected && !lit;
        // This board uses RGB, not the GRB order common on other WS2812 boards.
        let rgb = [0, if lit { GREEN_BRIGHTNESS } else { 0 }, 0];
        let mut pulses = [PulseCode::end_marker(); 26];
        for (index, pulse) in pulses[..24].iter_mut().enumerate() {
            let one = rgb[index / 8] & (0x80 >> (index % 8)) != 0;
            // 80 MHz ticks: each bit lasts 1.25 us, with 0.4/0.8 us high time.
            *pulse = if one {
                PulseCode::new(Level::High, 64, Level::Low, 36)
            } else {
                PulseCode::new(Level::High, 32, Level::Low, 68)
            };
        }
        // Hold low for 300 us to latch the color before the end marker.
        pulses[24] = PulseCode::new(Level::Low, 12_000, Level::Low, 12_000);
        let sent = matches!(
            with_timeout(Duration::from_millis(10), channel.transmit(&pulses)).await,
            Ok(Ok(()))
        );
        if !sent {
            println!("Status LED update failed");
        }

        connected = if connected && sent {
            WIFI_CONNECTED.wait().await
        } else {
            // Connection changes interrupt a blink immediately. A failed off
            // transmission is retried without disrupting Wi-Fi or meter reception.
            with_timeout(BLINK_INTERVAL, WIFI_CONNECTED.wait())
                .await
                .unwrap_or(connected)
        };
    }
}
