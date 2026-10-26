use crate::broker::{self, Family};
use crate::diagnostics::log;
use crate::settings::SettingsControl;
use crate::{UPDATED, config, status_led, telemetry};
use core::net::IpAddr;
use embassy_net::{Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, Interface, PowerSaveMode, WifiController, WifiError,
    sta::StationConfig,
};
use han_config::{MqttSettings, control::Status};
use han_core::Backoff;

const IO_TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Debug)]
enum Error {
    Connect(broker::Error),
    Mqtt(han_mqtt::Error),
}
#[embassy_executor::task]
pub(crate) async fn runner(mut runner: embassy_net::Runner<'static, Interface>) {
    runner.run().await;
}

#[embassy_executor::task]
pub(crate) async fn wifi(mut controller: WifiController<'static>) {
    let config = Config::Station(
        StationConfig::default()
            .with_ssid(config::WIFI_SSID.try_into().expect("validated Wi-Fi SSID"))
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                config::WIFI_PASSWORD
                    .try_into()
                    .expect("validated Wi-Fi password"),
            )),
    );
    let mut retry = Backoff::new();
    // Configure only once. A reconnect must not reconfigure a driver that may
    // still be completing a cancelled connection or disconnection.
    loop {
        match controller.set_config(&config) {
            Ok(()) => break,
            Err(error) => log!("Wi-Fi configuration failed: {:?}", error),
        }
        wifi_retry(&mut retry).await;
    }
    retry.reset();
    // Modem sleep reduces average P1 power demand; CPU/UART stay awake.
    let _ = controller.set_power_saving(PowerSaveMode::Minimum);
    loop {
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
                wifi_retry(&mut retry).await;
                continue;
            }
            Ok(Err(error)) => log!("Wi-Fi connection failed: {:?}", error),
            Err(_) => log!("Wi-Fi connection timed out; cancelling attempt"),
        }
        // Dropping connect_async only drops its event subscription. Explicitly
        // cancel the driver's Connecting or Connected state before retrying.
        match with_timeout(IO_TIMEOUT, controller.disconnect_async()).await {
            Ok(Ok(_)) | Ok(Err(WifiError::NotConnected)) => {}
            Ok(Err(error)) => log!("Wi-Fi cancellation failed: {:?}", error),
            Err(_) => log!("Wi-Fi cancellation timed out"),
        }
        wifi_retry(&mut retry).await;
    }
}

async fn wifi_retry(retry: &mut Backoff) {
    let delay = retry.next_seconds();
    log!("Wi-Fi retry in {}s", delay);
    Timer::after(Duration::from_secs(delay)).await;
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
                stack.wait_link_up().await;
                // SLAAC installs a link-local address without a router or DHCP.
                // All other destinations require an acquired IP configuration.
                if !matches!(state.settings.ip_address(), Some(IpAddr::V6(ip)) if ip.is_unicast_link_local()) {
                    stack.wait_config_up().await;
                }
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
                Error::Connect(broker::Error::Dns) => "Broker hostname could not be resolved.",
                Error::Connect(broker::Error::Tcp) => "Could not connect to the broker.",
                Error::Connect(broker::Error::Timeout) => "Broker connection timed out.",
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

async fn session<'s>(
    stack: Stack<'s>,
    id: &str,
    retry: &mut Backoff,
    socket: &mut TcpSocket<'s>,
    settings: &MqttSettings,
    control: &SettingsControl,
    revision: u64,
) -> Result<(), Error> {
    broker::connect(&mut BrokerTransport { stack, socket }, settings)
        .await
        .map_err(Error::Connect)?;

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

struct BrokerTransport<'a, 's> {
    stack: Stack<'s>,
    socket: &'a mut TcpSocket<'s>,
}

impl broker::Transport for BrokerTransport<'_, '_> {
    fn configured(&self, family: Family) -> bool {
        match family {
            Family::Ipv6 => self.stack.config_v6().is_some(),
            Family::Ipv4 => self.stack.config_v4().is_some(),
        }
    }

    async fn resolve(
        &mut self,
        host: &str,
        family: Family,
    ) -> Result<heapless::Vec<IpAddr, 4>, broker::Error> {
        let query = match family {
            Family::Ipv6 => DnsQueryType::Aaaa,
            Family::Ipv4 => DnsQueryType::A,
        };
        let addresses = with_timeout(Duration::from_secs(10), self.stack.dns_query(host, query))
            .await
            .map_err(|_| broker::Error::Timeout)?
            .map_err(|_| broker::Error::Dns)?;
        Ok(addresses.into_iter().map(Into::into).collect())
    }

    async fn connect(&mut self, address: IpAddr, port: u16) -> Result<(), broker::Error> {
        let result = with_timeout(
            IO_TIMEOUT,
            self.socket
                .connect((embassy_net::IpAddress::from(address), port)),
        )
        .await
        .map_err(|_| broker::Error::Timeout)
        .and_then(|result| result.map_err(|_| broker::Error::Tcp));
        if result.is_err() {
            // In particular, a timed-out SYN must not leave the socket in use
            // when the next IPv6 or IPv4 address is attempted.
            self.socket.abort();
            let _ = with_timeout(IO_TIMEOUT, self.socket.flush()).await;
        }
        result
    }
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
