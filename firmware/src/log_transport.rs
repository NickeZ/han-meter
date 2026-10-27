//! Own all uploader sockets on the MQTT/network executor. Only bounded byte
//! copies cross to the lower-priority uploader; embassy-net stays on one thread.
use crate::{broker, log_shipping::Config, network};
use alloc::vec::Vec;
use embassy_futures::select::{Either, select};
use embassy_net::{
    Stack,
    dns::DnsQueryType,
    tcp::TcpSocket,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Instant, with_timeout};
use esp_hal::rng::Rng;
use han_logs::{
    Endpoint,
    io_bridge::{Bridge, CHUNK_BYTES, Client, Server},
    ntp,
    ntp::client::{Error as NtpError, Family as NtpFamily},
};
use static_cell::StaticCell;

pub(crate) type Stream = Client<'static, CriticalSectionRawMutex, CHUNK_BYTES>;
type SocketOwner = Server<'static, CriticalSectionRawMutex, CHUNK_BYTES>;

pub(crate) struct Buffers {
    rx: Vec<u8>,
    tx: Vec<u8>,
}

enum Command {
    WaitConfig,
    Synchronize,
    Connect,
    Close,
}

enum Reply {
    Done,
    Time(Result<(u64, u64), NtpError>),
    Connected(Result<(), broker::Error>),
}

static COMMAND: Channel<CriticalSectionRawMutex, Command, 1> = Channel::new();
static REPLY: Channel<CriticalSectionRawMutex, Reply, 1> = Channel::new();

pub(crate) fn init() -> Option<(Stream, SocketOwner, Buffers)> {
    let buffers = Buffers {
        rx: crate::log_shipping::buffer(2048).ok()?,
        tx: crate::log_shipping::buffer(2048).ok()?,
    };
    static BRIDGE: StaticCell<Bridge<CriticalSectionRawMutex, CHUNK_BYTES>> = StaticCell::new();
    let (client, server) = BRIDGE.init_with(Bridge::new).split();
    Some((client, server, buffers))
}

// Control calls are serialized by the sole uploader and must run to completion.
// Only TLS byte I/O is cancelled, followed by an acknowledged Close before reuse.
pub(crate) async fn wait_config() {
    COMMAND.send(Command::WaitConfig).await;
    assert!(matches!(REPLY.receive().await, Reply::Done));
}

pub(crate) async fn synchronize() -> Result<(u64, u64), NtpError> {
    COMMAND.send(Command::Synchronize).await;
    let Reply::Time(result) = REPLY.receive().await else {
        unreachable!()
    };
    result
}

pub(crate) async fn connect() -> Result<(), broker::Error> {
    COMMAND.send(Command::Connect).await;
    let Reply::Connected(result) = REPLY.receive().await else {
        unreachable!()
    };
    result
}

pub(crate) async fn close() {
    COMMAND.send(Command::Close).await;
    assert!(matches!(REPLY.receive().await, Reply::Done));
}

#[embassy_executor::task]
pub(crate) async fn run(
    stack: Stack<'static>,
    config: Config,
    mut io: SocketOwner,
    mut buffers: Buffers,
) {
    let endpoint = Endpoint::parse(config.url).expect("validated VictoriaLogs URL");
    loop {
        match COMMAND.receive().await {
            Command::WaitConfig => {
                stack.wait_config_up().await;
                REPLY.send(Reply::Done).await;
            }
            Command::Synchronize => {
                let result = ntp::client::synchronize(&mut NtpTransport(stack), config.ntp_server)
                    .await
                    .map(|unix_ms| (unix_ms, Instant::now().as_millis()));
                REPLY.send(Reply::Time(result)).await;
            }
            Command::Connect => {
                let mut socket = TcpSocket::new(stack, &mut buffers.rx, &mut buffers.tx);
                socket.set_timeout(Some(Duration::from_secs(15)));
                let result = with_timeout(
                    Duration::from_secs(45),
                    network::connect_tcp(stack, &mut socket, endpoint.host(), endpoint.port()),
                )
                .await
                .unwrap_or(Err(broker::Error::Timeout));
                let connected = result.is_ok();
                REPLY.send(Reply::Connected(result)).await;
                if connected {
                    // Closing cancels pending reads/writes even if the peer is
                    // silent. The background TLS future has already been dropped.
                    let Either::First(command) =
                        select(COMMAND.receive(), io.serve(&mut socket)).await;
                    assert!(matches!(command, Command::Close));
                } else {
                    assert!(matches!(COMMAND.receive().await, Command::Close));
                }
                socket.abort();
                let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
                drop(socket);
                io.reset();
                REPLY.send(Reply::Done).await;
            }
            Command::Close => unreachable!("close requires a connection attempt"),
        }
    }
}

struct NtpTransport<'a>(Stack<'a>);

impl ntp::client::Transport for NtpTransport<'_> {
    fn configured(&self, family: NtpFamily) -> bool {
        match family {
            NtpFamily::Ipv6 => self.0.config_v6().is_some(),
            NtpFamily::Ipv4 => self.0.config_v4().is_some(),
        }
    }

    async fn resolve(
        &mut self,
        server: &str,
        family: NtpFamily,
    ) -> Result<heapless::Vec<core::net::IpAddr, 4>, NtpError> {
        let query = match family {
            NtpFamily::Ipv6 => DnsQueryType::Aaaa,
            NtpFamily::Ipv4 => DnsQueryType::A,
        };
        let found = with_timeout(Duration::from_secs(5), self.0.dns_query(server, query))
            .await
            .map_err(|_| NtpError::DnsTimeout(family))?
            .map_err(|_| NtpError::Dns(family))?;
        Ok(found.into_iter().map(Into::into).collect())
    }

    async fn query(&mut self, address: core::net::IpAddr) -> Result<u64, NtpError> {
        // A cancelled UDP send can leave a queued datagram. A fresh socket for
        // each address prevents an unreachable peer from blocking fallback.
        let mut rx_meta = [PacketMetadata::EMPTY; 1];
        let mut tx_meta = [PacketMetadata::EMPTY; 1];
        let mut rx = [0; 512];
        let mut tx = [0; 48];
        let mut socket = UdpSocket::new(self.0, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
        socket.bind(0).map_err(|_| NtpError::Bind)?;
        let mut nonce = [0; 8];
        Rng::new().read(&mut nonce);
        let request = ntp::request(nonce);
        let remote = embassy_net::IpEndpoint::new(address.into(), 123);
        let mut rejected = None;
        let result = with_timeout(Duration::from_secs(3), async {
            socket
                .send_to(&request, remote)
                .await
                .map_err(|_| NtpError::Send(address))?;
            let mut packet = [0; 512];
            loop {
                let (len, source) = socket
                    .recv_from(&mut packet)
                    .await
                    .map_err(|_| NtpError::Receive(address))?;
                if source.endpoint == remote {
                    match ntp::unix_ms(&packet[..len], nonce) {
                        Ok(time) => return Ok(time),
                        Err(reason) => rejected = Some(reason),
                    }
                }
            }
        })
        .await;
        result.unwrap_or(Err(match rejected {
            Some(reason) => NtpError::Rejected(address, reason),
            None => NtpError::Timeout(address),
        }))
    }
}
