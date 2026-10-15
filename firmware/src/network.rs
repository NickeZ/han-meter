use crate::diagnostics::log;
use crate::settings::SettingsControl;
use crate::{UPDATED, config, status_led, telemetry};
use embassy_net::{Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_radio::wifi::{Config, Interface, PowerSaveMode, WifiController, sta::StationConfig};
use han_config::{MqttSettings, control::Status};
use han_core::Backoff;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Debug)]
enum Error {
    Dns,
    Tcp,
    Timeout,
    Mqtt(han_mqtt::Error),
}
#[embassy_executor::task]
pub(crate) async fn runner(mut runner: embassy_net::Runner<'static, Interface<'static>>) {
    runner.run().await;
}

#[embassy_executor::task]
pub(crate) async fn wifi(mut controller: WifiController<'static>) {
    let config = Config::Station(
        StationConfig::default()
            .with_ssid(config::WIFI_SSID)
            .with_password(config::WIFI_PASSWORD.into()),
    );
    let mut retry = Backoff::new();
    loop {
        if let Err(error) = controller.set_config(&config) {
            log!("Wi-Fi configuration failed: {:?}", error);
        } else {
            // Modem sleep reduces average P1 power demand; CPU/UART stay awake.
            let _ = controller.set_power_saving(PowerSaveMode::Minimum);
            match with_timeout(Duration::from_secs(30), controller.connect_async()).await {
                Ok(Ok(_)) => {
                    status_led::set_wifi_connected(true);
                    log!("Wi-Fi connected");
                    let connected_at = Instant::now();
                    let _ = controller.wait_for_disconnect_async().await;
                    status_led::set_wifi_connected(false);
                    if connected_at.elapsed() >= Duration::from_secs(60) {
                        retry.reset();
                    }
                    log!("Wi-Fi disconnected");
                }
                _ => {
                    log!("Wi-Fi connection failed");
                    let _ = with_timeout(IO_TIMEOUT, controller.disconnect_async()).await;
                }
            }
        }
        let delay = retry.next_seconds();
        log!("Wi-Fi retry in {}s", delay);
        Timer::after(Duration::from_secs(delay)).await;
    }
}

pub(crate) async fn run(stack: Stack<'static>, id: &str, control: &'static SettingsControl) -> ! {
    let mut retry = Backoff::new();
    loop {
        let state = control.snapshot();
        let revision = state.revision;
        if !stack.is_config_up() {
            control.set_status(revision, Status::WaitingForWifi, None);
        }
        let mut rx = [0; 2048];
        let mut tx = [0; 2048];
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(Duration::from_secs(15)));
        let outcome = control
            .until_changed(revision, async {
                stack.wait_config_up().await;
                control.set_status(revision, Status::Connecting, None);
                session(
                    stack,
                    id,
                    &mut retry,
                    &mut socket,
                    &state.settings,
                    control,
                    revision,
                )
                .await
            })
            .await;
        // The socket lives outside the cancelled future. Flush its RST before
        // removal so the old broker can publish the retained offline Last Will.
        socket.abort();
        let _ = with_timeout(IO_TIMEOUT, socket.flush()).await;
        drop(socket);
        let result = match outcome {
            None => {
                retry.reset();
                continue;
            }
            Some(result) => result,
        };
        if let Err(error) = result {
            let message = match error {
                Error::Dns => "Broker hostname could not be resolved.",
                Error::Tcp => "Could not connect to the broker.",
                Error::Timeout => "Broker connection timed out.",
                Error::Mqtt(_) => "MQTT connection failed. Check the broker and credentials.",
            };
            control.set_status(revision, Status::Disconnected, Some(message));
            match error {
                Error::Mqtt(reason) => log!("MQTT session ended: {:?}", reason),
                _ => log!("MQTT connection failed: {:?}", error),
            }
        }
        let delay = retry.next_seconds();
        log!("MQTT retry in {}s", delay);
        if control
            .until_changed(revision, Timer::after(Duration::from_secs(delay)))
            .await
            .is_none()
        {
            retry.reset();
        }
    }
}

async fn session(
    stack: Stack<'_>,
    id: &str,
    retry: &mut Backoff,
    socket: &mut TcpSocket<'_>,
    settings: &MqttSettings,
    control: &SettingsControl,
    revision: u64,
) -> Result<(), Error> {
    let addresses = with_timeout(
        Duration::from_secs(10),
        stack.dns_query(&settings.host, DnsQueryType::A),
    )
    .await
    .map_err(|_| Error::Timeout)?
    .map_err(|_| Error::Dns)?;
    let address = *addresses.first().ok_or(Error::Dns)?;
    with_timeout(IO_TIMEOUT, socket.connect((address, settings.port)))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|_| Error::Tcp)?;

    let application = FirmwareApplication { control, revision };
    let config = han_mqtt::Config {
        id,
        device_name: config::DEVICE_NAME,
        version: env!("CARGO_PKG_VERSION"),
        username: &settings.username,
        password: &settings.password,
    };
    let started = Instant::now();
    let result = han_mqtt::run(socket, &config, &application).await;
    if started.elapsed() >= Duration::from_secs(60) {
        retry.reset();
    }
    result.map_err(Error::Mqtt)
}

struct FirmwareApplication<'a> {
    control: &'a SettingsControl,
    revision: u64,
}
impl han_mqtt::Application for FirmwareApplication<'_> {
    fn snapshot(&self) -> han_core::Telemetry {
        telemetry()
    }
    async fn changed(&self) {
        UPDATED.wait().await
    }
    fn connected(&self) {
        self.control
            .set_status(self.revision, Status::Connected, None);
        log!("MQTT connected");
    }
}
