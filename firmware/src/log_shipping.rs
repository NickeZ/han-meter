//! Bounded, retrying HTTPS export of the application diagnostic log.
use crate::{
    diagnostics::{self, log},
    network,
};
use alloc::{string::String, vec::Vec};
use core::cell::{Cell, RefCell};
use embassy_net::{
    Stack,
    dns::DnsQueryType,
    tcp::TcpSocket,
    udp::{PacketMetadata, UdpSocket},
};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::rng::{Rng, Trng};
use han_core::Backoff;
use han_logs::{
    BATCH_BYTES, Endpoint, Identity, ntp,
    ntp::client::{Error as NtpError, Family as NtpFamily},
    tls::{self, TlsClock},
};

#[derive(Clone, Copy)]
pub(crate) struct Config {
    pub url: &'static str,
    pub authorization: &'static str,
    pub ca_roots: &'static [&'static [u8]],
    pub ntp_server: &'static str,
}

struct Clock;
// (Unix milliseconds, uptime milliseconds at sync). No 64-bit atomic required.
static WALL_TIME: Mutex<CriticalSectionRawMutex, Cell<Option<(u64, u64)>>> =
    Mutex::new(Cell::new(None));
static TLS_RNG: Mutex<CriticalSectionRawMutex, RefCell<Option<Trng>>> =
    Mutex::new(RefCell::new(None));

struct CryptoRng;
impl embassy_crypto::driver::Rng for CryptoRng {
    fn fill_bytes(bytes: &mut [u8]) {
        TLS_RNG.lock(|rng| {
            rng.borrow()
                .as_ref()
                .expect("TLS entropy initialized")
                .read(bytes)
        });
    }
}
embassy_crypto::rng_impl!(CryptoRng);

impl Clock {
    fn now_ms() -> Option<u64> {
        let (unix, uptime) = WALL_TIME.lock(Cell::get)?;
        let elapsed = Instant::now().as_millis().checked_sub(uptime)?;
        // Stop sending if we cannot refresh the clock for a whole day.
        (elapsed < 24 * 60 * 60 * 1000)
            .then(|| unix.checked_add(elapsed))
            .flatten()
    }
}
impl TlsClock for Clock {
    fn now() -> Option<u64> {
        Self::now_ms().map(|time| time / 1000)
    }
}

fn buffer(size: usize) -> Result<Vec<u8>, ()> {
    let mut result = Vec::new();
    result.try_reserve_exact(size).map_err(|_| ())?;
    result.resize(size, 0);
    Ok(result)
}

#[derive(Debug)]
enum Error {
    Connect(crate::broker::Error),
    Upload(tls::Error),
    Timeout,
}

#[embassy_executor::task]
pub(crate) async fn run(stack: Stack<'static>, config: Config, device: String) {
    let endpoint = Endpoint::parse(config.url).expect("validated VictoriaLogs URL");
    // Wi-Fi owns the entropy source for the firmware lifetime. TLS never uses
    // the pseudorandom-only Rng as a cryptographic RNG.
    let trng = loop {
        if let Ok(rng) = Trng::try_new() {
            break rng;
        }
        Timer::after_secs(1).await;
    };
    TLS_RNG.lock(|rng| *rng.borrow_mut() = Some(trng));
    let mut tls = match tls::Client::<Clock>::new(config.ca_roots) {
        Ok(client) => client,
        Err(_) => {
            log!("VictoriaLogs disabled: insufficient TLS memory");
            return;
        }
    };
    let (mut body, mut rx, mut tx, mut scratch) = match (
        buffer(BATCH_BYTES),
        buffer(2048),
        buffer(2048),
        buffer(han_logs::HEADER_BYTES),
    ) {
        (Ok(body), Ok(rx), Ok(tx), Ok(scratch)) => (body, rx, tx, scratch),
        _ => {
            log!("VictoriaLogs disabled: insufficient buffer memory");
            return;
        }
    };
    let identity = Identity {
        device: &device,
        boot_id: diagnostics::boot_id(),
        firmware: env!("CARGO_PKG_VERSION"),
    };
    let mut after = 0;
    let mut retry = Backoff::new();
    // A clock failure must not suppress the next TLS/HTTP failure after time
    // synchronization recovers (for example an authentication rejection).
    let mut failed = false;
    let mut clock_error = None;
    let mut last_sync = None;
    let mut boot_unix_ms = None;
    log!("VictoriaLogs HTTPS export enabled");
    diagnostics::log_heap("log buffers allocated");
    loop {
        stack.wait_config_up().await;
        if let Err(reason) =
            refresh_clock(stack, config.ntp_server, &mut last_sync, &mut boot_unix_ms).await
        {
            if clock_error != Some(reason) {
                log!("VictoriaLogs waiting for NTP: {:?}", reason);
                clock_error = Some(reason);
            }
            Timer::after_secs(retry.next_seconds()).await;
            continue;
        }
        clock_error = None;
        let page = diagnostics::log_page(after);
        if page.entries.is_empty() {
            Timer::after_secs(5).await;
            continue;
        }
        let len = match han_logs::encode_batch(&page, after, &identity, boot_unix_ms, &mut body) {
            Ok(len) => len,
            Err(_) => {
                log!("VictoriaLogs disabled: batch exceeds its buffer");
                return;
            }
        };
        let next_after = page.next_after;
        let more = page.more;
        drop(page);
        // Keep these exact bytes and this cursor until acknowledged. Dropping a
        // timed-out TLS future destroys its session; the next attempt starts fresh.
        loop {
            stack.wait_config_up().await;
            if let Err(reason) =
                refresh_clock(stack, config.ntp_server, &mut last_sync, &mut boot_unix_ms).await
            {
                if clock_error != Some(reason) {
                    log!("VictoriaLogs waiting for NTP: {:?}", reason);
                    clock_error = Some(reason);
                }
                Timer::after_secs(retry.next_seconds()).await;
                continue;
            }
            clock_error = None;
            let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
            socket.set_timeout(Some(Duration::from_secs(15)));
            let result = with_timeout(Duration::from_secs(45), async {
                network::connect_tcp(stack, &mut socket, endpoint.host(), endpoint.port())
                    .await
                    .map_err(Error::Connect)?;
                tls.post(
                    &mut socket,
                    &endpoint,
                    config.authorization,
                    &body[..len],
                    scratch.as_mut_slice().try_into().unwrap(),
                )
                .await
                .map_err(Error::Upload)
            })
            .await
            .unwrap_or(Err(Error::Timeout));
            socket.abort();
            let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
            drop(socket);
            match result {
                Ok(()) => {
                    if after == 0 {
                        log!("VictoriaLogs first log batch delivered");
                    }
                    after = next_after;
                    retry.reset();
                    if failed {
                        log!("VictoriaLogs delivery recovered");
                        failed = false;
                    }
                    break;
                }
                Err(error) => {
                    if !failed {
                        // Only bounded error codes; never log credentials, request
                        // headers, URL query strings, or the server response body.
                        match error {
                            Error::Connect(reason) => {
                                log!("VictoriaLogs connection failed: {:?}", reason)
                            }
                            Error::Upload(reason) => {
                                log!("VictoriaLogs upload failed: {:?}", reason)
                            }
                            Error::Timeout => log!("VictoriaLogs upload timed out"),
                        }
                        failed = true;
                    }
                    Timer::after_secs(retry.next_seconds()).await;
                }
            }
        }
        if !more {
            Timer::after_secs(5).await;
        }
    }
}

async fn refresh_clock(
    stack: Stack<'_>,
    server: &str,
    last_sync: &mut Option<Instant>,
    boot_unix_ms: &mut Option<u64>,
) -> Result<(), NtpError> {
    if last_sync.is_some_and(|last| last.elapsed() < Duration::from_secs(6 * 60 * 60)) {
        return Ok(());
    }
    let unix = ntp::client::synchronize(&mut NtpTransport(stack), server).await?;
    let uptime = Instant::now().as_millis();
    WALL_TIME.lock(|state| state.set(Some((unix, uptime))));
    boot_unix_ms.get_or_insert(unix.saturating_sub(uptime));
    *last_sync = Some(Instant::now());
    log!("NTP synchronized: unix_ms={}", unix);
    Ok(())
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
