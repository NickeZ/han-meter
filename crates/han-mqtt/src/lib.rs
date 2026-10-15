#![no_std]
#![forbid(unsafe_code)]

use core::{fmt::Write, future::Future, num::NonZeroU16};
use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use han_core::{
    FIELD_COUNT, FIELDS, Telemetry,
    discovery::{self, HA_STATUS_TOPIC, Topics},
};
use rust_mqtt::{
    Bytes,
    buffer::AllocBuffer,
    client::{
        Client,
        event::Event,
        options::{
            ConnectOptions, PublicationOptions, SubscriptionOptions, TopicReference, WillOptions,
        },
    },
    config::KeepAlive,
    io::Transport,
    types::{MqttBinary, MqttString, TopicFilter, TopicName},
};

pub struct Config<'a> {
    pub id: &'a str,
    pub device_name: &'a str,
    pub version: &'a str,
    pub username: &'a str,
    pub password: &'a str,
}

pub trait Application {
    fn snapshot(&self) -> Telemetry;
    fn changed(&self) -> impl Future<Output = ()>;
    fn connected(&self) {}
    /// Monotonic milliseconds in the same time domain as the snapshot's timestamps.
    fn now_ms(&self) -> u64 {
        Instant::now().as_millis()
    }
}

type Mqtt<'a, 'b, N> = Client<'a, 'b, N, AllocBuffer, 1, 1, 1, 0, 4, 0, 0>;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub enum Error {
    Mqtt,
    Timeout,
    Rejected,
    Encoding,
    PingTimeout,
}

struct Publishing {
    discovered: u32,
    sent: [u64; FIELD_COUNT],
    field_online: [Option<bool>; FIELD_COUNT],
    device_online: Option<bool>,
    refresh: bool,
    ping_sent: Option<Instant>,
}

impl Publishing {
    fn new() -> Self {
        Self {
            discovered: 0,
            sent: [0; FIELD_COUNT],
            field_online: [None; FIELD_COUNT],
            device_online: None,
            refresh: true,
            ping_sent: None,
        }
    }

    fn event(&mut self, event: Event<'_, 0, 4>) -> Result<(), Error> {
        match event {
            Event::Pingresp => self.ping_sent = None,
            Event::Publish(publish) => {
                if let TopicReference::Name(topic) = publish.topic
                    && topic.as_ref().as_str() == HA_STATUS_TOPIC
                    && publish.message.as_bytes() == b"online"
                {
                    self.refresh = true;
                }
            }
            Event::PublishRejected(_) => return Err(Error::Rejected),
            Event::Suback(ack) if ack.reason_code.is_erroneous() => return Err(Error::Rejected),
            _ => {}
        }
        Ok(())
    }
}

pub async fn run<N: Transport, A: Application>(
    network: N,
    config: &Config<'_>,
    application: &A,
) -> Result<(), Error> {
    let id = config.id;
    let availability = discovery::availability_topic(id).map_err(|_| Error::Encoding)?;
    let will = WillOptions::new(
        topic(&availability)?,
        MqttBinary::try_from("offline").unwrap(),
    )
    .at_least_once()
    .retain();
    let options = ConnectOptions::new()
        .clean_start()
        .keep_alive(KeepAlive::Seconds(NonZeroU16::new(30).unwrap()))
        .maximum_packet_size(core::num::NonZeroU32::new(1024).unwrap())
        .user_name(MqttString::try_from(config.username).map_err(|_| Error::Encoding)?)
        .password(MqttBinary::try_from(config.password).map_err(|_| Error::Encoding)?)
        .will(will);
    let mut buffer = AllocBuffer;
    let mut client: Mqtt<'_, '_, N> = Client::new(&mut buffer);
    with_timeout(
        IO_TIMEOUT,
        client.connect(
            network,
            &options,
            Some(MqttString::try_from(id).map_err(|_| Error::Encoding)?),
        ),
    )
    .await
    .map_err(|_| Error::Timeout)?
    .map_err(|_| Error::Mqtt)?;
    let mut publishing = Publishing::new();

    // Override any retained online value before restoring current measurements.
    publish(
        &mut client,
        &availability,
        b"offline",
        true,
        &mut publishing,
    )
    .await?;
    let filter = TopicFilter::new(MqttString::try_from(HA_STATUS_TOPIC).unwrap()).unwrap();
    with_timeout(IO_TIMEOUT, async {
        client
            .subscribe(filter, &SubscriptionOptions::new())
            .await
            .map_err(|_| Error::Mqtt)?;
        loop {
            let event = client.poll().await.map_err(|_| Error::Mqtt)?;
            if let Event::Suback(ack) = &event {
                return if ack.reason_code.is_success() {
                    Ok(())
                } else {
                    Err(Error::Rejected)
                };
            }
            publishing.event(event)?;
        }
    })
    .await
    .map_err(|_| Error::Timeout)??;

    application.connected();

    let keep_alive_ms = match client.shared_config().keep_alive {
        KeepAlive::Seconds(seconds) => u64::from(seconds.get()) * 1000,
        KeepAlive::Infinite => 30_000,
    };
    let ping_interval = Duration::from_millis((keep_alive_ms / 2).min(10_000));
    let mut last_ping = Instant::now();
    let mut next_tick = Instant::now();
    loop {
        with_timeout(
            Duration::from_secs(10),
            synchronize(
                &mut client,
                config,
                application,
                &availability,
                &mut publishing,
            ),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        if publishing
            .ping_sent
            .is_some_and(|sent| sent.elapsed().as_millis() >= keep_alive_ms)
        {
            return Err(Error::PingTimeout);
        }
        if publishing.ping_sent.is_none() && last_ping.elapsed() >= ping_interval {
            with_timeout(IO_TIMEOUT, client.ping())
                .await
                .map_err(|_| Error::Timeout)?
                .map_err(|_| Error::Mqtt)?;
            last_ping = Instant::now();
            publishing.ping_sent = Some(last_ping);
        }
        if Instant::now() >= next_tick {
            next_tick = Instant::now() + Duration::from_millis(500);
        }

        // Only poll_header is cancel-safe. Once a header arrives, finish the body or
        // drop the entire connection on timeout/error; never resume a partial body.
        match select3(
            client.poll_header(),
            application.changed(),
            Timer::at(next_tick),
        )
        .await
        {
            Either3::First(header) => {
                let header = header.map_err(|_| Error::Mqtt)?;
                let event = with_timeout(IO_TIMEOUT, client.poll_body(header))
                    .await
                    .map_err(|_| Error::Timeout)?
                    .map_err(|_| Error::Mqtt)?;
                publishing.event(event)?;
            }
            Either3::Second(()) | Either3::Third(()) => {}
        }
    }
    // Dropping the client drops/aborts its TCP socket. No normal MQTT DISCONNECT:
    // the broker must publish our offline Will on errors and timeouts.
}

fn topic(value: &str) -> Result<TopicName<'_>, Error> {
    TopicName::new(MqttString::try_from(value).map_err(|_| Error::Encoding)?).ok_or(Error::Encoding)
}

async fn publish<N: Transport>(
    client: &mut Mqtt<'_, '_, N>,
    name: &str,
    payload: &[u8],
    retained: bool,
    publishing: &mut Publishing,
) -> Result<(), Error> {
    with_timeout(IO_TIMEOUT, async {
        let mut options = PublicationOptions::new(TopicReference::Name(topic(name)?));
        if retained {
            options = options.retain().at_least_once();
        }
        let identifier = client
            .publish(&options, Bytes::from(payload))
            .await
            .map_err(|_| Error::Mqtt)?;
        if let Some(identifier) = identifier {
            loop {
                let event = client.poll().await.map_err(|_| Error::Mqtt)?;
                if let Event::PublishAcknowledged(ack) = &event
                    && ack.packet_identifier == identifier
                {
                    break;
                }
                publishing.event(event)?;
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| Error::Timeout)?
}

async fn synchronize<N: Transport, A: Application>(
    client: &mut Mqtt<'_, '_, N>,
    config: &Config<'_>,
    application: &A,
    availability: &str,
    publishing: &mut Publishing,
) -> Result<(), Error> {
    let id = config.id;
    if core::mem::take(&mut publishing.refresh) {
        publishing.discovered = 0;
        publishing.sent = [0; FIELD_COUNT];
        publishing.field_online = [None; FIELD_COUNT];
        publishing.device_online = None;
    }
    let snapshot = application.snapshot();
    let mut buffer = [0; discovery::DISCOVERY_CAPACITY];
    for (i, field) in FIELDS.iter().enumerate() {
        let topics = Topics::new(id, field).map_err(|_| Error::Encoding)?;
        let Some(reading) = snapshot.readings[i] else {
            // Clear stale retained availability even for fields not yet seen on this boot.
            if publishing.field_online[i] != Some(false) {
                publish(client, &topics.availability, b"offline", true, publishing).await?;
                publishing.field_online[i] = Some(false);
            }
            continue;
        };
        if publishing.discovered & (1 << i) == 0 {
            let len = discovery::encode(
                id,
                config.device_name,
                config.version,
                field,
                &topics,
                availability,
                &mut buffer,
            )
            .map_err(|_| Error::Encoding)?;
            publish(client, &topics.discovery, &buffer[..len], true, publishing).await?;
            publishing.discovered |= 1 << i;
        }
        let fresh = reading.is_fresh(application.now_ms());
        if fresh && publishing.sent[i] != reading.generation {
            let mut value = heapless::String::<48>::new();
            write!(value, "{}", reading.value).map_err(|_| Error::Encoding)?;
            publish(client, &topics.state, value.as_bytes(), false, publishing).await?;
            publishing.sent[i] = reading.generation;
        }
        // Independent availability prevents a missing field from staying fresh due
        // to other readings, or a replay from extending its original 60 s deadline.
        let fresh = reading.is_fresh(application.now_ms());
        if publishing.field_online[i] != Some(fresh) {
            publish(
                client,
                &topics.availability,
                if fresh { b"online" } else { b"offline" },
                true,
                publishing,
            )
            .await?;
            publishing.field_online[i] = Some(fresh);
        }
    }
    let online = snapshot.online(application.now_ms());
    if publishing.device_online != Some(online) {
        publish(
            client,
            availability,
            if online { b"online" } else { b"offline" },
            true,
            publishing,
        )
        .await?;
        publishing.device_online = Some(online);
    }
    Ok(())
}
