use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    device_name: String,
    wifi: Wifi,
    mqtt: Mqtt,
    victorialogs: Option<VictoriaLogs>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VictoriaLogs {
    url: String,
    username: String,
    password: String,
    #[serde(default = "default_ntp")]
    ntp_server: String,
}
fn default_ntp() -> String {
    "pool.ntp.org".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wifi {
    ssid: String,
    password: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mqtt {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    username: String,
    password: String,
}
fn default_port() -> u16 {
    1883
}

fn check_text(name: &str, value: &str, min: usize, max: usize) {
    assert!(
        (min..=max).contains(&value.len()),
        "{name} must contain {min}..={max} bytes"
    );
    assert!(
        !value.chars().any(char::is_control),
        "{name} must not contain control characters"
    );
}

fn main() {
    println!("cargo:rustc-link-arg=-Tlinkall.x");
    println!("cargo:rerun-if-changed=memory.x");
    println!(
        "cargo:rustc-link-arg=-T{}/memory.x",
        env::var("CARGO_MANIFEST_DIR").unwrap()
    );
    println!("cargo:rerun-if-env-changed=HAN_CONFIG");
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("..");
    let path = env::var_os("HAN_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    println!("cargo:rerun-if-changed={}", path.display());
    let input = fs::read_to_string(&path).unwrap_or_else(|_| panic!(
        "Cannot read configuration file {}. Copy config.example.toml to config.toml and edit it, or set HAN_CONFIG.", path.display()));
    // Do not print TOML parser errors: they can include source lines containing credentials.
    let config: Config = toml::from_str(&input).unwrap_or_else(|_| panic!(
        "Invalid configuration TOML: check required fields/types against config.example.toml; unknown fields are rejected"));
    check_text("device_name", &config.device_name, 1, 64);
    check_text("wifi.ssid", &config.wifi.ssid, 1, 32);
    check_text("wifi.password", &config.wifi.password, 8, 64);
    if config.wifi.password.len() == 64 {
        assert!(
            config.wifi.password.bytes().all(|b| b.is_ascii_hexdigit()),
            "A 64-byte Wi-Fi password must be a hexadecimal PSK"
        );
    }
    han_config::MqttSettings::new(
        &config.mqtt.host,
        config.mqtt.port,
        &config.mqtt.username,
        &config.mqtt.password,
    )
    .unwrap_or_else(|error| panic!("Invalid MQTT configuration: {error}"));
    let mut generated = String::new();
    for (key, value) in [
        ("DEVICE_NAME", &config.device_name),
        ("WIFI_SSID", &config.wifi.ssid),
        ("WIFI_PASSWORD", &config.wifi.password),
        ("MQTT_HOST", &config.mqtt.host),
        ("MQTT_USERNAME", &config.mqtt.username),
        ("MQTT_PASSWORD", &config.mqtt.password),
    ] {
        // Rust's Debug string escaping prevents configuration from becoming Rust code.
        generated.push_str(&format!("pub const {key}: &str = {value:?};\n"));
    }
    generated.push_str(&format!(
        "pub const MQTT_PORT: u16 = {};\n",
        config.mqtt.port
    ));
    if let Some(logs) = config.victorialogs {
        let endpoint = han_logs::Endpoint::parse(&logs.url)
            .unwrap_or_else(|_| panic!("victorialogs.url must be an HTTPS URL with an ingest path and no embedded credentials"));
        assert!(
            endpoint.host().len() <= 64,
            "victorialogs.url hostname must be at most 64 bytes for embedded-tls verification"
        );
        let authorization = han_logs::basic_auth(&logs.username, &logs.password)
            .unwrap_or_else(|_| panic!("Invalid VictoriaLogs credentials: username 1..=128 bytes without colon, password 1..=256 bytes; no control characters"));
        let ntp_url = format!("https://{}/", logs.ntp_server);
        let ntp = han_logs::Endpoint::parse(&ntp_url).unwrap_or_else(|_| {
            panic!(
                "victorialogs.ntp_server must be a DNS hostname or IP address (brackets for IPv6)"
            )
        });
        assert!(
            ntp.path() == "/"
                && (ntp.authority() == ntp.host()
                    || ntp.authority() == format!("[{}]", ntp.host())),
            "Invalid victorialogs.ntp_server"
        );
        let roots = [
            include_str!("certs/isrg-root-x1.pem"),
            include_str!("certs/isrg-root-x2.pem"),
        ]
        .map(|pem| {
            let base64: String = pem
                .lines()
                .filter(|line| !line.starts_with("-----"))
                .collect();
            STANDARD.decode(base64).expect("bundled ISRG root PEM")
        });
        println!("cargo:rerun-if-changed=certs/isrg-root-x1.pem");
        println!("cargo:rerun-if-changed=certs/isrg-root-x2.pem");
        generated.push_str(&format!(
            "pub const LOGGING: Option<crate::log_shipping::Config> = Some(crate::log_shipping::Config {{ url: {:?}, authorization: {:?}, ca_roots: &[&{:?}, &{:?}], ntp_server: {:?} }});\n",
            logs.url, authorization.as_str(), roots[0], roots[1], ntp.host()
        ));
    } else {
        generated.push_str("pub const LOGGING: Option<crate::log_shipping::Config> = None;\n");
    }
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("config.rs"),
        generated,
    )
    .unwrap();
}
