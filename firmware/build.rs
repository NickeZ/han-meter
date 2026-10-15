use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    device_name: String,
    wifi: Wifi,
    mqtt: Mqtt,
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
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("config.rs"),
        generated,
    )
    .unwrap();
}
