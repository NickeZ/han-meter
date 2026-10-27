use core::convert::Infallible;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embedded_io_async::{ErrorType, Read, Write};
use han_config::diagnostics::{LOG_CAPACITY, Log, PAGE_SIZE};
use han_logs::{
    BATCH_BYTES, Endpoint, HEADER_BYTES, Identity, basic_auth, encode_batch, http, ntp,
};

#[test]
fn configuration_rejects_plaintext_credentials_in_urls_and_header_injection() {
    for url in [
        "http://logs.test/insert/jsonline",
        "https://user:pass@logs.test/insert/jsonline",
        "https://logs.test/insert\r\nX:bad",
        "https://logs.test:0/",
        "https://logs.test:65536/",
        "https://logs.test/x#fragment",
        "https://logs.test",
        "https://bad_host/",
        "https://[fe80::1%wlan0]/",
    ] {
        assert!(Endpoint::parse(url).is_err(), "{url:?}");
    }
    let endpoint =
        Endpoint::parse("https://logs.test:8443/prefix/insert/jsonline?tenant=one").unwrap();
    assert_eq!(
        (endpoint.host(), endpoint.port(), endpoint.path()),
        ("logs.test", 8443, "/prefix/insert/jsonline?tenant=one")
    );
    let endpoint = Endpoint::parse("https://[2001:db8::1]:443/insert/jsonline").unwrap();
    assert_eq!(endpoint.host(), "2001:db8::1");
    assert_eq!(endpoint.authority(), "[2001:db8::1]:443");
    assert_eq!(basic_auth("user", "pass").unwrap(), "Basic dXNlcjpwYXNz");
    assert!(basic_auth("a:b", "pass").is_err());
    assert!(basic_auth("user", "pass\r\nX: value").is_err());
    assert!(basic_auth("", "pass").is_err());
    assert_eq!(
        basic_auth(&"u".repeat(128), &"p".repeat(256))
            .unwrap()
            .len(),
        522
    );
}

#[test]
fn jsonlines_preserve_order_escape_messages_timestamp_and_report_loss() {
    let log = Log::<NoopRawMutex>::new();
    for n in 0..LOG_CAPACITY + 3 {
        log.record(n as u64, format_args!("event {n}: \"quoted\"\nline\tå🦀"));
    }
    let identity = Identity {
        device: "han-aabb",
        boot_id: 42,
        firmware: "test",
    };
    let page = log.page(0);
    let mut buffer = [0; BATCH_BYTES];
    let len = encode_batch(&page, 0, &identity, Some(1_800_000_000_000), &mut buffer).unwrap();
    let lines: Vec<serde_json::Value> = core::str::from_utf8(&buffer[..len])
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), PAGE_SIZE);
    assert_eq!(lines[0]["lost_entries"], 3);
    assert_eq!(lines[0]["sequence"], 4);
    assert_eq!(lines[0]["_time"], 1_800_000_000_003u64);
    assert_eq!(lines[0]["_msg"], "event 3: \"quoted\"\nline\tå🦀");
    assert!(lines[1].get("lost_entries").is_none());
    assert_eq!(lines[0]["device"], "han-aabb");
    assert_eq!(lines[0]["boot_id"], 42);
    assert!(encode_batch(&page, 0, &identity, None, &mut [0; 5]).is_err());
    // A retained batch is independent of the live ring during a long outage.
    let retained = buffer[..len].to_vec();
    for n in 0..LOG_CAPACITY * 2 {
        log.record(n as u64, format_args!("later {n}"));
    }
    assert_eq!(&buffer[..len], retained);
    assert!(log.page(page.next_after).lost);
}

#[test]
fn worst_case_escaped_page_fits_bounded_batch() {
    let log = Log::<NoopRawMutex>::new();
    for _ in 0..PAGE_SIZE {
        log.record(u64::MAX, format_args!("{}", "\u{0001}".repeat(160)));
    }
    let mut output = [0; BATCH_BYTES];
    let page = log.page(0);
    let identity = Identity {
        device: "han-ffffffffffff",
        boot_id: u32::MAX,
        firmware: "0.1.0",
    };
    let len = encode_batch(&page, 0, &identity, None, &mut output).unwrap();
    assert!(len < BATCH_BYTES);
    for line in core::str::from_utf8(&output[..len]).unwrap().lines() {
        let _: serde_json::Value = serde_json::from_str(line).unwrap();
    }
}

#[test]
fn ntp_requires_matching_synchronized_server_response_and_handles_era_rollover() {
    let nonce = [0x55; 8];
    assert_eq!(ntp::request(nonce)[40..48], nonce);
    let mut reply = [0; 48];
    reply[0] = 0x24;
    reply[1] = 2;
    reply[24..32].copy_from_slice(&nonce);
    let unix = 1_800_000_000u64;
    reply[40..44].copy_from_slice(&((unix + 2_208_988_800) as u32).to_be_bytes());
    reply[44..48].copy_from_slice(&0x8000_0000u32.to_be_bytes());
    assert_eq!(ntp::unix_ms(&reply, nonce), Ok(unix * 1000 + 500));
    for len in 0..48 {
        assert_eq!(
            ntp::unix_ms(&reply[..len], nonce),
            Err(ntp::ResponseError::ShortPacket)
        );
    }
    assert_eq!(
        ntp::unix_ms(&reply, [0; 8]),
        Err(ntp::ResponseError::Origin)
    );
    for (index, byte, reason) in [
        (0, 0xe4, ntp::ResponseError::Unsynchronized),
        (0, 0x23, ntp::ResponseError::Mode),
        (0, 0x14, ntp::ResponseError::Version),
        (1, 16, ntp::ResponseError::Stratum),
    ] {
        let mut bad = reply;
        bad[index] = byte;
        assert_eq!(ntp::unix_ms(&bad, nonce), Err(reason));
    }
    let mut denied = reply;
    denied[1] = 0;
    denied[12..16].copy_from_slice(b"RATE");
    assert_eq!(
        ntp::unix_ms(&denied, nonce),
        Err(ntp::ResponseError::KissOfDeath(*b"RATE"))
    );
    // Even a denial must match the request before interpreting its reason.
    assert_eq!(
        ntp::unix_ms(&denied, [0; 8]),
        Err(ntp::ResponseError::Origin)
    );
    let mut zero = reply;
    zero[40..48].fill(0);
    assert_eq!(
        ntp::unix_ms(&zero, nonce),
        Err(ntp::ResponseError::ZeroTimestamp)
    );
    let mut old = reply;
    old[40..44].copy_from_slice(&3_786_825_599u32.to_be_bytes());
    assert_eq!(
        ntp::unix_ms(&old, nonce),
        Err(ntp::ResponseError::DateBefore2020)
    );
    let unix_2040 = 2_208_988_800u64;
    reply[40..44].copy_from_slice(&((unix_2040 + 2_208_988_800) as u32).to_be_bytes());
    assert_eq!(ntp::unix_ms(&reply, nonce), Ok(unix_2040 * 1000 + 500));
}

struct Stream {
    sent: Vec<u8>,
    response: Vec<u8>,
    offset: usize,
}
impl ErrorType for Stream {
    type Error = Infallible;
}
impl Write for Stream {
    async fn write(&mut self, data: &[u8]) -> Result<usize, Infallible> {
        let count = data.len().min(3);
        self.sent.extend_from_slice(&data[..count]);
        Ok(count)
    }
    async fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}
impl Read for Stream {
    async fn read(&mut self, output: &mut [u8]) -> Result<usize, Infallible> {
        if self.offset == self.response.len() {
            return Ok(0);
        }
        output[0] = self.response[self.offset];
        self.offset += 1;
        Ok(1)
    }
}

#[tokio::test]
async fn http_handles_short_io_status_errors_redirects_and_incomplete_responses() {
    let endpoint = Endpoint::parse("https://logs.test:8443/insert/jsonline?tenant=1").unwrap();
    let body = b"{\"_msg\":\"hello\"}\n";
    for (response, expected) in [
        (
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_owned(),
            Ok(()),
        ),
        (
            "HTTP/1.1 401 Unauthorized\r\n\r\n".to_owned(),
            Err(http::Error::Status(401)),
        ),
        (
            "HTTP/1.1 429 Too Many Requests\r\n\r\n".to_owned(),
            Err(http::Error::Status(429)),
        ),
        (
            "HTTP/1.1 503 Unavailable\r\n\r\n".to_owned(),
            Err(http::Error::Status(503)),
        ),
        (
            "HTTP/1.1 302 Redirect\r\nLocation: https://other.test/\r\n\r\n".to_owned(),
            Err(http::Error::Status(302)),
        ),
        ("HTTP/1.1 200 OK\r\n".to_owned(), Err(http::Error::Response)),
        (
            "HTTP/1.1 200X\r\n\r\n".to_owned(),
            Err(http::Error::Response),
        ),
        (
            format!("HTTP/1.1 200 OK\r\nX: {}\r\n\r\n", "x".repeat(2048)),
            Err(http::Error::Response),
        ),
    ] {
        let mut stream = Stream {
            sent: Vec::new(),
            response: response.into_bytes(),
            offset: 0,
        };
        assert_eq!(
            http::post(
                &mut stream,
                &endpoint,
                "Basic dXNlcjpwYXNz",
                body,
                &mut [0; HEADER_BYTES]
            )
            .await,
            expected
        );
        let request = core::str::from_utf8(&stream.sent).unwrap();
        assert!(
            request
                .starts_with("POST /insert/jsonline?tenant=1 HTTP/1.1\r\nHost: logs.test:8443\r\n")
        );
        assert!(request.contains("Authorization: Basic dXNlcjpwYXNz\r\n"));
        assert!(request.contains("Content-Type: application/stream+json\r\n"));
        assert!(request.contains("VL-Stream-Fields: device\r\n"));
        assert!(request.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(stream.sent.ends_with(body));
    }
}
