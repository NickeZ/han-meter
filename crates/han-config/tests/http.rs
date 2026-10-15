use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use han_config::{
    MqttSettings,
    control::Control,
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
}
impl App {
    fn new() -> Self {
        Self {
            control: Control::new(MqttSettings::new("broker", 1883, "meter", "secret").unwrap()),
            failing: Cell::new(false),
            writes: Cell::new(0),
        }
    }
}
impl Application for App {
    fn snapshot(&self) -> han_config::control::Snapshot {
        self.control.snapshot()
    }
    fn storage_available(&self) -> bool {
        !self.failing.get()
    }
    fn host_allowed(&self, host: &str) -> bool {
        matches!(host, "device" | "device:80")
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
        let _ = picoserve::Server::new_tokio(&router, &config, &mut buffer)
            .serve(socket)
            .await;
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
