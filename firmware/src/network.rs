use crate::{UPDATED, config, telemetry};
use embassy_net::{Stack, dns::DnsQueryType, tcp::TcpSocket};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_println::println;
use esp_radio::wifi::{Config, Interface, PowerSaveMode, WifiController, sta::StationConfig};
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
            println!("Wi-Fi configuration failed: {:?}", error);
        } else {
            // Modem sleep reduces average P1 power demand; CPU/UART stay awake.
            let _ = controller.set_power_saving(PowerSaveMode::Minimum);
            match with_timeout(Duration::from_secs(30), controller.connect_async()).await {
                Ok(Ok(_)) => {
                    println!("Wi-Fi connected");
                    let connected_at = Instant::now();
                    let _ = controller.wait_for_disconnect_async().await;
                    if connected_at.elapsed() >= Duration::from_secs(60) {
                        retry.reset();
                    }
                    println!("Wi-Fi disconnected");
                }
                _ => {
                    println!("Wi-Fi connection failed");
                    let _ = with_timeout(IO_TIMEOUT, controller.disconnect_async()).await;
                }
            }
        }
        let delay = retry.next_seconds();
        println!("Wi-Fi retry in {}s", delay);
        Timer::after(Duration::from_secs(delay)).await;
    }
}

pub(crate) async fn run(stack: Stack<'static>, id: &str) -> ! {
    let mut retry = Backoff::new();
    loop {
        stack.wait_config_up().await;
        if let Err(error) = session(stack, id, &mut retry).await {
            match error {
                Error::Mqtt(reason) => println!("MQTT session ended: {:?}", reason),
                _ => println!("MQTT connection failed: {:?}", error),
            }
        }
        let delay = retry.next_seconds();
        println!("MQTT retry in {}s", delay);
        Timer::after(Duration::from_secs(delay)).await;
    }
}

async fn session(stack: Stack<'_>, id: &str, retry: &mut Backoff) -> Result<(), Error> {
    let addresses = with_timeout(
        Duration::from_secs(10),
        stack.dns_query(config::MQTT_HOST, DnsQueryType::A),
    )
    .await
    .map_err(|_| Error::Timeout)?
    .map_err(|_| Error::Dns)?;
    let address = *addresses.first().ok_or(Error::Dns)?;
    let mut rx = [0; 2048];
    let mut tx = [0; 2048];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(Duration::from_secs(15)));
    with_timeout(IO_TIMEOUT, socket.connect((address, config::MQTT_PORT)))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|_| Error::Tcp)?;

    let application = FirmwareApplication;
    let config = han_mqtt::Config {
        id,
        device_name: config::DEVICE_NAME,
        version: env!("CARGO_PKG_VERSION"),
        username: config::MQTT_USERNAME,
        password: config::MQTT_PASSWORD,
    };
    let started = Instant::now();
    let result = han_mqtt::run(socket, &config, &application).await;
    if started.elapsed() >= Duration::from_secs(60) {
        retry.reset();
    }
    result.map_err(Error::Mqtt)
}

struct FirmwareApplication;
impl han_mqtt::Application for FirmwareApplication {
    fn snapshot(&self) -> han_core::Telemetry {
        telemetry()
    }
    async fn changed(&self) {
        UPDATED.wait().await
    }
    fn connected(&self) {
        println!("MQTT connected");
    }
}
