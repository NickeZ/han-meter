//! Small, bounded HTTP interface shared by the firmware and host tests.
use crate::{
    MqttSettings,
    control::{Snapshot, Status},
};
use heapless::String;
use picoserve::{
    Router,
    extract::{FromRequest, FromRequestParts, State},
    request::{RequestBody, RequestParts},
    response::{Json, StatusCode},
    routing::{PathRouter, get},
};
use serde::{Deserialize, Serialize};

pub const BODY_LIMIT: usize = 4096;
pub const HTTP_BUFFER_SIZE: usize = 6144;

pub trait Application {
    fn snapshot(&self) -> Snapshot;
    fn save(&self, settings: MqttSettings) -> Result<bool, SaveError>;
    fn storage_available(&self) -> bool;
    /// Accept the current device IP or advertised DHCP hostname (optional :80).
    fn host_allowed(&self, host: &str) -> bool;
}

#[derive(Clone, Copy, Debug)]
pub struct SaveError;

#[derive(Serialize)]
struct View {
    host: String<253>,
    port: u16,
    username: String<128>,
    password_set: bool,
    status: Status,
    last_error: Option<&'static str>,
    storage_available: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    host: String<253>,
    port: u16,
    username: String<128>,
    password: Option<String<256>>,
}

pub fn parse_update(body: &[u8], previous: &MqttSettings) -> Result<MqttSettings, &'static str> {
    finish_update(decode_update(body)?, previous)
}

fn decode_update(body: &[u8]) -> Result<Update, &'static str> {
    if body.len() > BODY_LIMIT {
        return Err("Request body is too large.");
    }
    let (update, used): (Update, _) = serde_json_core::from_slice_escaped(body, &mut [0; 256])
        .map_err(|_| "Invalid settings JSON or field length.")?;
    if !body[used..].iter().all(u8::is_ascii_whitespace) {
        return Err("Invalid settings JSON.");
    }
    Ok(update)
}

fn finish_update(update: Update, previous: &MqttSettings) -> Result<MqttSettings, &'static str> {
    let password = update
        .password
        .as_ref()
        .filter(|p| !p.is_empty())
        .map_or(previous.password.as_str(), |p| p.as_str());
    MqttSettings::new(&update.host, update.port, &update.username, password)
        .map_err(|error| error.message())
}

struct LocalRequest;
struct WriteRequest;
type Rejection = (StatusCode, &'static str);

struct Body(Update);
impl<'r, S> FromRequest<'r, S> for Body {
    type Rejection = Rejection;
    async fn from_request<R: picoserve::io::Read>(
        _: &'r S,
        _: RequestParts<'r>,
        body: RequestBody<'r, R>,
    ) -> Result<Self, Rejection> {
        let bytes = body
            .read_all()
            .await
            .map_err(|_| (StatusCode::BAD_REQUEST, "Could not read request body."))?;
        decode_update(bytes)
            .map(Self)
            .map_err(|message| (StatusCode::BAD_REQUEST, message))
    }
}

struct Html;
impl picoserve::response::Content for Html {
    fn content_type(&self) -> &'static str {
        "text/html; charset=utf-8"
    }
    fn content_length(&self) -> usize {
        include_str!("settings.html").len()
    }
    async fn write_content<W: picoserve::io::Write>(self, mut writer: W) -> Result<(), W::Error> {
        writer.write_all(include_bytes!("settings.html")).await
    }
}

impl<'r, A: Application> FromRequestParts<'r, &A> for LocalRequest {
    type Rejection = Rejection;
    async fn from_request_parts(app: &'r &A, parts: &RequestParts<'r>) -> Result<Self, Rejection> {
        let host = parts
            .headers()
            .get("host")
            .and_then(|v| v.as_str().ok())
            .ok_or((StatusCode::BAD_REQUEST, "Missing Host header."))?;
        if !app.host_allowed(host) {
            return Err((
                StatusCode::FORBIDDEN,
                "Use the device IP address or DHCP hostname.",
            ));
        }
        Ok(Self)
    }
}

impl<'r, A: Application> FromRequestParts<'r, &A> for WriteRequest {
    type Rejection = Rejection;
    async fn from_request_parts(app: &'r &A, parts: &RequestParts<'r>) -> Result<Self, Rejection> {
        LocalRequest::from_request_parts(app, parts).await?;
        let headers = parts.headers();
        let header = |name| headers.get(name).and_then(|v| v.as_str().ok());
        if header("content-type")
            .and_then(|v| v.split(';').next())
            .map(str::trim)
            != Some("application/json")
        {
            return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send application/json."));
        }
        // A custom header prevents cross-site simple requests. No CORS permission
        // is granted, and a browser Origin must also match the local Host.
        if header("x-han-settings") != Some("1") {
            return Err((StatusCode::FORBIDDEN, "Missing settings request header."));
        }
        if let Some(origin) = header("origin") {
            let host = header("host")
                .unwrap_or("")
                .strip_suffix(":80")
                .unwrap_or(header("host").unwrap_or(""));
            if origin
                .strip_prefix("http://")
                .map(|v| v.strip_suffix(":80").unwrap_or(v))
                != Some(host)
            {
                return Err((
                    StatusCode::FORBIDDEN,
                    "Cross-origin settings changes are not allowed.",
                ));
            }
        }
        if headers.get("transfer-encoding").is_some() {
            return Err((
                StatusCode::BAD_REQUEST,
                "Transfer encoding is not supported.",
            ));
        }
        let length = header("content-length")
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or((StatusCode::LENGTH_REQUIRED, "Content-Length is required."))?;
        if length > BODY_LIMIT {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "Request body is too large."));
        }
        Ok(Self)
    }
}

#[derive(Serialize)]
struct SaveResponse {
    saved: bool,
    changed: bool,
    message: &'static str,
}

pub fn router<A: Application>(app: &A) -> Router<impl PathRouter + '_> {
    Router::new()
        .route("/", get(async |_: LocalRequest| {
            Html
        }))
        .route("/api/mqtt", get(async |_: LocalRequest, State(app): State<&A>| {
            let state = app.snapshot();
            Json(View {
                host: state.settings.host,
                port: state.settings.port,
                username: state.settings.username,
                password_set: !state.settings.password.is_empty(),
                status: state.status,
                last_error: state.last_error,
                storage_available: app.storage_available(),
            }).into_response().with_header("Cache-Control", "no-store")
        }).post(async |_: WriteRequest, State(app): State<&A>, Body(update): Body| {
            let result = finish_update(update, &app.snapshot().settings);
            let (status, response) = match result {
                Err(message) => (StatusCode::BAD_REQUEST, SaveResponse { saved: false, changed: false, message }),
                Ok(settings) => match app.save(settings) {
                    Ok(changed) => (StatusCode::OK, SaveResponse { saved: true, changed, message: if changed {
                        "Settings saved. Reconnecting MQTT."
                    } else { "Settings are unchanged." } }),
                    Err(_) => (StatusCode::SERVICE_UNAVAILABLE, SaveResponse {
                        saved: false, changed: false,
                        message: "Could not save settings to flash. Current settings are still active; restart the device before retrying.",
                    }),
                },
            };
            Json(response).into_response().with_status_code(status).with_header("Cache-Control", "no-store")
        }))
        .with_state(app)
}
