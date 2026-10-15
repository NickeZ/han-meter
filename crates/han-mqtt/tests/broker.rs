//! Exercises the production MQTT session against an isolated Mosquitto broker.
//! The observer is an independent, minimal MQTT 3.1.1 wire client.
use embedded_io_adapters::tokio_1::FromTokio;
use han_core::{Decimal, Snapshot, Telemetry, Value, discovery};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Notify,
    task::LocalSet,
};

#[derive(Clone)]
struct App {
    telemetry: Rc<RefCell<Telemetry>>,
    now: Rc<Cell<u64>>,
    notify: Rc<Notify>,
}

impl App {
    fn new() -> Self {
        Self {
            telemetry: Rc::new(RefCell::new(Telemetry::new())),
            now: Rc::new(Cell::new(100_000)),
            notify: Rc::new(Notify::new()),
        }
    }
    fn update(&self, energy: Option<&str>, power: &str) {
        let mut snapshot = Snapshot::default();
        snapshot.readings[0] = energy.map(|v| Value::Number(Decimal::parse(v).unwrap()));
        snapshot.readings[4] = Some(Value::Number(Decimal::parse(power).unwrap()));
        self.telemetry.borrow_mut().update(snapshot, self.now.get());
        self.notify.notify_one();
    }
}

impl han_mqtt::Application for App {
    fn snapshot(&self) -> Telemetry {
        *self.telemetry.borrow()
    }
    async fn changed(&self) {
        self.notify.notified().await
    }
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
}

fn config<'a>(id: &'a str, password: &'a str) -> han_mqtt::Config<'a> {
    han_mqtt::Config {
        id,
        device_name: "Integration meter",
        version: "test",
        username: "han-test",
        password,
    }
}

fn mqtt_string(output: &mut Vec<u8>, value: &str) {
    output.extend_from_slice(&(value.len() as u16).to_be_bytes());
    output.extend_from_slice(value.as_bytes());
}

struct Observer(TcpStream);

#[derive(Debug)]
struct Message {
    topic: String,
    payload: String,
    retained: bool,
}

impl Observer {
    async fn connect(address: &str, id: &str, filters: &[&str]) -> Self {
        let mut this = Self(TcpStream::connect(address).await.unwrap());
        let mut body = vec![0, 4, b'M', b'Q', b'T', b'T', 4, 0xc2, 0, 0];
        mqtt_string(&mut body, id);
        mqtt_string(&mut body, "han-test");
        mqtt_string(&mut body, "test-password");
        this.send(0x10, &body).await;
        assert_eq!(this.packet().await, (0x20, vec![0, 0]));
        let mut subscribe = vec![0, 1];
        for &filter in filters {
            mqtt_string(&mut subscribe, filter);
            subscribe.push(0); // QoS 0 for the observer; no acknowledgements needed.
        }
        this.send(0x82, &subscribe).await;
        let (header, body) = this.packet().await;
        assert_eq!(header, 0x90);
        assert!(body[2..].iter().all(|q| *q == 0));
        this
    }

    async fn send(&mut self, header: u8, body: &[u8]) {
        let mut packet = vec![header];
        let mut len = body.len();
        loop {
            let mut byte = (len % 128) as u8;
            len /= 128;
            if len != 0 {
                byte |= 128;
            }
            packet.push(byte);
            if len == 0 {
                break;
            }
        }
        packet.extend_from_slice(body);
        self.0.write_all(&packet).await.unwrap();
    }

    async fn packet(&mut self) -> (u8, Vec<u8>) {
        tokio::time::timeout(Duration::from_secs(10), async {
            let header = self.0.read_u8().await.unwrap();
            let mut len = 0usize;
            let mut multiplier = 1;
            loop {
                let byte = self.0.read_u8().await.unwrap();
                len += usize::from(byte & 127) * multiplier;
                if byte & 128 == 0 {
                    break;
                }
                multiplier *= 128;
                assert!(multiplier <= 128 * 128 * 128);
            }
            assert!(len < 8192);
            let mut body = vec![0; len];
            self.0.read_exact(&mut body).await.unwrap();
            (header, body)
        })
        .await
        .expect("broker observation timed out")
    }

    async fn message(&mut self) -> Message {
        let (header, body) = self.packet().await;
        assert_eq!(header & 0xf6, 0x30); // PUBLISH, QoS 0
        let len = u16::from_be_bytes([body[0], body[1]]) as usize;
        Message {
            topic: String::from_utf8(body[2..2 + len].to_vec()).unwrap(),
            payload: String::from_utf8(body[2 + len..].to_vec()).unwrap(),
            retained: header & 1 != 0,
        }
    }

    async fn until(&mut self, done: impl Fn(&[Message]) -> bool) -> Vec<Message> {
        let mut messages = Vec::new();
        while !done(&messages) {
            messages.push(self.message().await);
            assert!(messages.len() < 200, "too many messages: {messages:?}");
        }
        messages
    }

    async fn publish(&mut self, topic: &str, payload: &str) {
        let mut body = Vec::new();
        mqtt_string(&mut body, topic);
        body.extend_from_slice(payload.as_bytes());
        self.send(0x30, &body).await;
    }
}

fn contains(messages: &[Message], topic: &str, payload: &str) -> bool {
    messages
        .iter()
        .any(|m| m.topic == topic && m.payload == payload)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an isolated broker; run scripts/test-mqtt.sh"]
async fn discovery_freshness_restart_last_will_and_authentication() {
    LocalSet::new()
        .run_until(async {
            let address = std::env::var("HAN_TEST_BROKER").expect("HAN_TEST_BROKER");
            let id = discovery::device_id([0x02, 0, 0, 0, 0, 1]);
            let availability = discovery::availability_topic(&id).unwrap();
            let energy = discovery::Topics::new(&id, &han_core::FIELDS[0]).unwrap();
            let power = discovery::Topics::new(&id, &han_core::FIELDS[4]).unwrap();
            let mut observer = Observer::connect(
                &address,
                "observer",
                &["han_meter/#", "homeassistant/sensor/+/config"],
            )
            .await;
            let app = App::new();
            app.update(Some("12345678.123"), "1.234");
            let device = launch(&address, &id, app.clone()).await;

            let messages = observer
                .until(|m| contains(m, &availability, "online"))
                .await;
            assert!(contains(&messages, &energy.state, "12345678.123"));
            assert!(contains(&messages, &power.state, "1.234"));
            let energy_config = messages
                .iter()
                .find(|m| m.topic == energy.discovery.as_str())
                .unwrap();
            let discovery: serde_json::Value =
                serde_json::from_str(&energy_config.payload).unwrap();
            assert_eq!(discovery["unique_id"], energy.unique_id.as_str());
            assert_eq!(discovery["state_class"], "total_increasing");
            assert_eq!(discovery["device"]["identifiers"][0], id.as_str());

            // A new subscriber receives discovery and availability, never an old measurement.
            let mut replay = Observer::connect(
                &address,
                "replay",
                &[
                    &availability,
                    &energy.discovery,
                    &energy.availability,
                    &energy.state,
                ],
            )
            .await;
            let replayed = replay.until(|m| m.len() == 3).await;
            assert!(replayed.iter().all(|m| m.retained));
            assert!(replayed.iter().all(|m| m.topic != energy.state.as_str()));

            // Time advances in the application domain; only power is refreshed.
            app.now.set(160_001);
            app.update(None, "2.345");
            let messages = observer
                .until(|m| {
                    contains(m, &energy.availability, "offline")
                        && contains(m, &power.state, "2.345")
                })
                .await;
            assert!(!messages.iter().any(|m| m.topic == energy.state.as_str()));

            observer.publish("homeassistant/status", "online").await;
            let messages = observer
                .until(|m| contains(m, &availability, "online"))
                .await;
            assert!(
                messages
                    .iter()
                    .any(|m| m.topic == energy.discovery.as_str())
            );
            assert!(contains(&messages, &energy.availability, "offline"));
            assert!(contains(&messages, &power.state, "2.345"));
            assert!(!messages.iter().any(|m| m.topic == energy.state.as_str()));

            // Dropping the production session must result in the broker's offline Will.
            device.abort();
            assert!(device.await.unwrap_err().is_cancelled());
            observer
                .until(|m| contains(m, &availability, "offline"))
                .await;

            let device = launch(&address, &id, app.clone()).await;
            let messages = observer
                .until(|m| contains(m, &availability, "online"))
                .await;
            assert!(contains(&messages, &power.state, "2.345"));
            assert!(!messages.iter().any(|m| m.topic == energy.state.as_str()));
            assert!(
                messages
                    .iter()
                    .any(|m| m.topic == energy.discovery.as_str())
            );

            // All telemetry can expire while the MQTT connection remains healthy.
            app.now.set(220_002);
            app.notify.notify_one();
            let messages = observer
                .until(|m| contains(m, &availability, "offline"))
                .await;
            assert!(contains(&messages, &power.availability, "offline"));
            assert!(!device.is_finished());
            device.abort();
            let _ = device.await;
            observer
                .until(|m| contains(m, &availability, "offline"))
                .await;

            // Reboot with empty RAM clears retained availability for previously seen fields.
            let rebooted = App::new();
            rebooted.update(None, "3.456");
            let device = launch(&address, &id, rebooted).await;
            let messages = observer
                .until(|m| contains(m, &availability, "online"))
                .await;
            assert!(contains(&messages, &energy.availability, "offline"));
            assert!(contains(&messages, &power.state, "3.456"));
            device.abort();
            let _ = device.await;

            let socket = FromTokio::new(TcpStream::connect(&address).await.unwrap());
            let config = config("bad-password-test", "wrong-password");
            assert!(han_mqtt::run(socket, &config, &App::new()).await.is_err());
        })
        .await;
}

async fn launch(address: &str, id: &str, app: App) -> tokio::task::JoinHandle<()> {
    let socket = FromTokio::new(TcpStream::connect(address).await.unwrap());
    let id = id.to_owned();
    tokio::task::spawn_local(async move {
        let config = config(&id, "test-password");
        let result = han_mqtt::run(socket, &config, &app).await;
        panic!("device session unexpectedly ended: {result:?}");
    })
}
