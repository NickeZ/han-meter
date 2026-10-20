use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use han_config::{
    MqttSettings,
    control::Control,
    diagnostics::{Log, Snapshot as Diagnostics, UartErrors},
    web::{self, Application, SaveError},
};
use std::cell::Cell;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

struct App {
    control: Control<NoopRawMutex>,
    failing: Cell<bool>,
    writes: Cell<usize>,
    log: Log<NoopRawMutex>,
}
impl App {
    fn new() -> Self {
        Self {
            control: Control::new(MqttSettings::new("broker", 1883, "meter", "secret").unwrap()),
            failing: Cell::new(false),
            writes: Cell::new(0),
            log: Log::new(),
        }
    }
}
impl Application for App {
    fn snapshot(&self) -> han_config::control::Snapshot {
        self.control.snapshot()
    }
    fn diagnostics(&self, after: u64) -> Diagnostics {
        Diagnostics {
            boot_id: 123,
            uptime_ms: 65_000,
            valid_frames: 60,
            invalid_frames: 2,
            uart_errors: 45,
            uart_error_types: UartErrors {
                overflow: 40,
                framing: 5,
                ..Default::default()
            },
            dma: han_config::diagnostics::DmaStats {
                received_bytes: 12345,
                errors: 2,
                overruns: 1,
            },
            last_valid_age_ms: Some(250),
            last_interval_ms: Some(1000),
            max_interval_ms: Some(3000),
            log: self.log.page(after),
        }
    }
    fn storage_available(&self) -> bool {
        !self.failing.get()
    }
    fn host_allowed(&self, host: &str) -> bool {
        web::host_allowed(
            host,
            "device",
            [
                "192.0.2.2".parse().unwrap(),
                "2001:db8::80".parse().unwrap(),
            ],
        )
    }
    fn save(&self, settings: MqttSettings) -> Result<bool, SaveError> {
        if self.failing.get() {
            return Err(SaveError);
        }
        let changed = self.control.apply(settings);
        if changed {
            self.writes.set(self.writes.get() + 1);
        }
        Ok(changed)
    }
}

async fn exchange(app: &App, request: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = async {
        let (socket, _) = listener.accept().await.unwrap();
        let router = web::router(app);
        let config = picoserve::Config::new(picoserve::Timeouts {
            start_read_request: picoserve::time::Duration::from_millis(200),
            persistent_start_read_request: picoserve::time::Duration::from_millis(200),
            read_request: picoserve::time::Duration::from_millis(200),
            write: picoserve::time::Duration::from_secs(1),
        });
        let mut buffer = [0; web::HTTP_BUFFER_SIZE];
        let connection = picoserve::Server::new_tokio(&router, &config, &mut buffer).serve(socket);
        // Large owned responses are duplicated through the router's nested
        // futures. Host execution tolerates this; the ESP32's stack does not.
        let bytes = core::mem::size_of_val(&connection);
        assert!(
            bytes <= 32 * 1024,
            "HTTP future grew to {bytes} bytes; keep responses indirect"
        );
        let _ = connection.await;
    };
    let client = async {
        let mut socket = TcpStream::connect(address).await.unwrap();
        // Exercise requests split across multiple reads, including the JSON body.
        for bytes in request.as_bytes().chunks(37) {
            if socket.write_all(bytes).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let mut response = Vec::new();
        let _ = socket.read_to_end(&mut response).await;
        String::from_utf8(response).unwrap()
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(server, client).1
    })
    .await
    .unwrap()
}

fn post(body: &str) -> String {
    format!(
        "POST /api/mqtt HTTP/1.1\r\nHost: device\r\nOrigin: http://device\r\nX-Han-Settings: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn http_authorities_only_accept_local_addresses_and_the_http_port() {
    let allowed = |host| {
        web::host_allowed(
            host,
            "device",
            [
                "192.0.2.2".parse().unwrap(),
                "2001:db8::80".parse().unwrap(),
            ],
        )
    };
    for host in [
        "device",
        "DEVICE:80",
        "192.0.2.2:80",
        "[2001:db8::80]",
        "[2001:db8::80]:80",
        "[2001:0db8:0:0:0:0:0:80]",
    ] {
        assert!(allowed(host), "{host}");
    }
    for host in [
        "evil.example",
        "192.0.2.3",
        "[2001:db8::81]",
        "2001:db8::80",
        "[2001:db8::80]:8080",
        "[2001:db8::80",
        "[192.0.2.2]",
        "[2001:db8::80%wlan0]",
        "device:80:80",
    ] {
        assert!(!allowed(host), "{host}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ipv6_authorities_and_broker_settings_work_with_origin_checks() {
    let app = App::new();
    let request = post(r#"{"host":"[fd12:3456:789a::10]","port":1883,"username":"meter"}"#)
        .replace("Host: device", "Host: [2001:db8::80]:80")
        .replace("Origin: http://device", "Origin: http://[2001:db8::80]");
    let response = exchange(&app, &request).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(
        app.snapshot().settings.ip_address(),
        Some("fd12:3456:789a::10".parse().unwrap())
    );
    assert_eq!(app.snapshot().settings.password, "secret");
    let response = exchange(
        &app,
        "GET /api/mqtt HTTP/1.1\r\nHost: [2001:db8::80]\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("[fd12:3456:789a::10]"));
    let response = exchange(
        &app,
        &request.replace(
            "Origin: http://[2001:db8::80]",
            "Origin: http://[2001:db8::81]",
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403"));
    assert_eq!(app.writes.get(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn settings_page_and_api_apply_updates_without_exposing_passwords() {
    let app = App::new();
    let page = exchange(&app, "GET / HTTP/1.1\r\nHost: device\r\n\r\n").await;
    assert!(page.starts_with("HTTP/1.1 200"));
    assert!(page.contains("text/html"));
    assert!(page.contains("Save and reconnect"));
    let response = exchange(&app, "GET /api/mqtt HTTP/1.1\r\nHost: device\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(
        response
            .to_ascii_lowercase()
            .contains("cache-control: no-store")
    );
    assert!(!response.contains("secret"));
    assert!(!response.contains("\"password\":"));
    assert!(response.contains("\"password_set\":true"));
    let update =
        post(r#"{"host":"new-broker","port":1884,"username":"new-user","password":"new-secret"}"#);
    let response = exchange(&app, &update).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("\"saved\":true"));
    assert!(!response.contains("new-secret"));
    assert_eq!(app.snapshot().settings.password, "new-secret");
    assert_eq!(app.snapshot().settings.port, 1884);
    let response = exchange(&app, &update).await;
    assert!(response.contains("\"changed\":false"));
    assert_eq!(app.writes.get(), 1);
    let response = exchange(
        &app,
        &post(r#"{"host":"third-broker","port":1883,"username":"user","password":""}"#),
    )
    .await;
    assert!(response.contains("\"saved\":true"));
    assert_eq!(app.snapshot().settings.password, "new-secret");
}

#[tokio::test(flavor = "current_thread")]
async fn rejects_cross_origin_invalid_and_oversized_requests_without_changes() {
    let app = App::new();
    let update = post(r#"{"host":"new-broker","port":1884,"username":"new-user"}"#);
    for request in [
        update.replace("Origin: http://device", "Origin: http://evil.example"),
        update.replace("Host: device", "Host: evil.example"),
        update.replace("X-Han-Settings: 1\r\n", ""),
    ] {
        assert!(exchange(&app, &request).await.starts_with("HTTP/1.1 403"));
    }
    let response = exchange(&app, &update.replace("application/json", "text/plain")).await;
    assert!(response.starts_with("HTTP/1.1 415"));
    let response = exchange(&app, &post(&" ".repeat(4097))).await;
    assert!(response.starts_with("HTTP/1.1 413"));
    let response = exchange(
        &app,
        &post(r#"{"host":"broker","port":0,"username":"meter","password":"secret"}"#),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 400"));
    assert!(!response.contains("secret"));
    let response = exchange(&app, &post("{broken")).await;
    assert!(response.starts_with("HTTP/1.1 400"));
    assert_eq!(app.writes.get(), 0);
    assert_eq!(app.snapshot().settings.port, 1883);
}

#[tokio::test(flavor = "current_thread")]
async fn storage_failure_and_incomplete_request_preserve_active_settings() {
    let app = App::new();
    app.failing.set(true);
    let update = post(r#"{"host":"new-broker","port":1884,"username":"new-user"}"#);
    let response = exchange(&app, &update).await;
    assert!(response.starts_with("HTTP/1.1 503"));
    assert_eq!(app.snapshot().settings.host, "broker");
    assert_eq!(app.writes.get(), 0);
    app.failing.set(false);
    // A client that stalls mid-body must not monopolize the worker or save a prefix.
    let partial = &update[..update.len() - 10];
    let response = exchange(&app, partial).await;
    assert!(!response.starts_with("HTTP/1.1 200"));
    assert_eq!(app.writes.get(), 0);
    let response = exchange(&app, &update).await;
    assert!(response.starts_with("HTTP/1.1 200"));
}

#[tokio::test(flavor = "current_thread")]
async fn diagnostics_are_paginated_uncached_and_exclude_credentials() {
    let app = App::new();
    for i in 1..=20 {
        app.log
            .record(i * 1000, format_args!("UART FifoOverflowed; count={i}"));
    }
    // Diagnostics remain usable even when settings storage has failed.
    app.failing.set(true);
    let response = exchange(
        &app,
        "GET /api/diagnostics HTTP/1.1\r\nHost: device\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(
        response
            .to_ascii_lowercase()
            .contains("cache-control: no-store")
    );
    let body: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["uart_errors"], 45);
    assert_eq!(body["uart_error_types"]["overflow"], 40);
    assert_eq!(body["dma"]["received_bytes"], 12345);
    assert_eq!(body["dma"]["errors"], 2);
    assert_eq!(body["dma"]["overruns"], 1);
    assert_eq!(body["max_interval_ms"], 3000);
    assert_eq!(body["log"]["entries"].as_array().unwrap().len(), 4);
    assert_eq!(body["log"]["more"], true);
    assert_eq!(body["log"]["next_after"], 4);
    for private in ["secret", "broker", "username", "password"] {
        assert!(!response.contains(private));
    }
    let response = exchange(
        &app,
        "GET /api/diagnostics?after=16 HTTP/1.1\r\nHost: device\r\n\r\n",
    )
    .await;
    let body: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["log"]["entries"].as_array().unwrap().len(), 4);
    assert_eq!(body["log"]["entries"][0]["sequence"], 17);
    assert_eq!(body["log"]["more"], false);
    assert_eq!(body["log"]["next_after"], 20);
    let response = exchange(
        &app,
        "GET /api/diagnostics?after=bad HTTP/1.1\r\nHost: device\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 400"));
    let response = exchange(
        &app,
        "GET /api/diagnostics HTTP/1.1\r\nHost: evil.example\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403"));
    assert!(!response.contains("FifoOverflowed"));
    assert_eq!(app.writes.get(), 0);
}
