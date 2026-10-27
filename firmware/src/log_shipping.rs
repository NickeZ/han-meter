//! Bounded, retrying HTTPS export of the application diagnostic log.
use crate::{
    diagnostics::{self, log},
    log_transport,
};
use alloc::{string::String, vec::Vec};
use core::cell::{Cell, RefCell};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Instant, Timer, with_deadline};
use esp_hal::rng::Trng;
use han_core::Backoff;
use han_logs::{
    BATCH_BYTES, Endpoint, Identity,
    ntp::client::Error as NtpError,
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

pub(crate) fn buffer(size: usize) -> Result<Vec<u8>, ()> {
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
pub(crate) async fn run(config: Config, device: String, mut stream: log_transport::Stream) {
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
    let (mut body, mut scratch) = match (buffer(BATCH_BYTES), buffer(han_logs::HEADER_BYTES)) {
        (Ok(body), Ok(scratch)) => (body, scratch),
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
    log!(
        "VictoriaLogs HTTPS export enabled; background priority={}, MQTT priority={}",
        crate::scheduling::BACKGROUND_PRIORITY,
        crate::scheduling::MQTT_PRIORITY
    );
    let heap = esp_alloc::HEAP.stats();
    log!(
        "Heap log buffers allocated: free={}B min_free={}B total={}B",
        heap.size - heap.current_usage,
        heap.size - heap.max_usage,
        heap.size
    );
    loop {
        log_transport::wait_config().await;
        if let Err(reason) = refresh_clock(&mut last_sync, &mut boot_unix_ms).await {
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
            log_transport::wait_config().await;
            if let Err(reason) = refresh_clock(&mut last_sync, &mut boot_unix_ms).await {
                if clock_error != Some(reason) {
                    log!("VictoriaLogs waiting for NTP: {:?}", reason);
                    clock_error = Some(reason);
                }
                Timer::after_secs(retry.next_seconds()).await;
                continue;
            }
            clock_error = None;
            let deadline = Instant::now() + Duration::from_secs(45);
            // Control requests always finish; only the TLS future is cancelled.
            // Close waits for the socket owner to cancel I/O and discard queued
            // bytes before the next TLS session can use the bridge.
            let result = match log_transport::connect().await {
                Ok(()) => with_deadline(
                    deadline,
                    tls.post(
                        &mut stream,
                        &endpoint,
                        config.authorization,
                        &body[..len],
                        scratch.as_mut_slice().try_into().unwrap(),
                    ),
                )
                .await
                .map_err(|_| Error::Timeout)
                .and_then(|result| result.map_err(Error::Upload)),
                Err(reason) => Err(Error::Connect(reason)),
            };
            log_transport::close().await;
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
    last_sync: &mut Option<Instant>,
    boot_unix_ms: &mut Option<u64>,
) -> Result<(), NtpError> {
    if last_sync.is_some_and(|last| last.elapsed() < Duration::from_secs(6 * 60 * 60)) {
        return Ok(());
    }
    // Timestamp on the network thread: foreground work may delay this thread
    // after the SNTP response arrives, without making the wall clock run late.
    let (unix, uptime) = log_transport::synchronize().await?;
    WALL_TIME.lock(|state| state.set(Some((unix, uptime))));
    boot_unix_ms.get_or_insert(unix.saturating_sub(uptime));
    *last_sync = Some(Instant::from_millis(uptime));
    log!("NTP synchronized: unix_ms={}", unix);
    Ok(())
}
