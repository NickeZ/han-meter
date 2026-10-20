use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use han_config::{
    MqttSettings, ValidationError,
    control::{Control, Status},
    web::parse_update,
};
use std::{
    future::Future,
    task::{Context, Poll, Waker},
};

fn settings() -> MqttSettings {
    MqttSettings::new("broker.localdomain", 1883, "meter", "secret").unwrap()
}

#[test]
fn validation_bounds_and_errors_do_not_contain_values() {
    assert!(
        MqttSettings::new(
            &"a".repeat(253),
            u16::MAX,
            &"u".repeat(128),
            &"p".repeat(256)
        )
        .is_ok()
    );
    for host in [
        "",
        "mqtt://secret",
        "broker:1883",
        "a b",
        "å",
        &"h".repeat(254),
    ] {
        assert!(matches!(
            MqttSettings::new(host, 1883, "u", "p"),
            Err(ValidationError::Host)
        ));
    }
    assert!(matches!(
        MqttSettings::new("broker", 0, "u", "p"),
        Err(ValidationError::Port)
    ));
    for username in ["", "user\nname", &"u".repeat(129), &"å".repeat(65)] {
        assert!(matches!(
            MqttSettings::new("broker", 1883, username, "p"),
            Err(ValidationError::Username)
        ));
    }
    for password in ["", "secret\0", &"p".repeat(257)] {
        let error = MqttSettings::new("broker", 1883, "u", password)
            .err()
            .unwrap();
        assert_eq!(error, ValidationError::Password);
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn ipv6_literals_are_validated_and_parsed_without_dns_or_zone_ids() {
    for (host, address) in [
        ("192.0.2.10", "192.0.2.10"),
        ("2001:db8::10", "2001:db8::10"),
        ("[2001:db8::10]", "2001:db8::10"),
        ("fd12:3456:789a::80", "fd12:3456:789a::80"),
        ("fe80::1234", "fe80::1234"),
        ("::ffff:192.0.2.10", "::ffff:192.0.2.10"),
    ] {
        let settings = MqttSettings::new(host, 1883, "u", "p").unwrap();
        assert_eq!(settings.ip_address(), Some(address.parse().unwrap()));
        assert_eq!(settings.host, host);
    }
    assert_eq!(settings().ip_address(), None);
    for host in [
        "2001:db8:::10",
        "2001:db8::gg",
        "[2001:db8::10]:1883",
        "[2001:db8::10",
        "2001:db8::10]",
        "[]",
        "[broker]",
        "[192.0.2.10]",
        "fe80::10%wlan0",
        "[fe80::10%25wlan0]",
        "mqtt://[2001:db8::10]",
        " 2001:db8::10",
    ] {
        assert!(
            matches!(
                MqttSettings::new(host, 1883, "u", "p"),
                Err(ValidationError::Host)
            ),
            "{host}"
        );
    }
}

#[test]
fn json_keeps_password_and_handles_escaped_credentials() {
    let previous = settings();
    for suffix in ["", ",\"password\":null", ",\"password\":\"\""] {
        let json = format!(r#"{{"host":"new-broker","port":1884,"username":"new-user"{suffix}}}"#);
        let next = parse_update(json.as_bytes(), &previous).unwrap();
        assert_eq!(next.password, "secret");
        assert_eq!(next.port, 1884);
    }
    let next = parse_update(
        br#"{"host":"broker","port":1883,"username":"us\u00e9r","password":"a\"b\\c&<="}"#,
        &previous,
    )
    .unwrap();
    assert_eq!(next.username, "usér");
    assert_eq!(next.password, "a\"b\\c&<=");
    let max = serde_json::json!({"host":"h".repeat(253),"port":65535,"username":"u".repeat(128),"password":"\\".repeat(256)}).to_string();
    assert!(parse_update(max.as_bytes(), &previous).is_ok());
}

#[test]
fn malformed_json_never_includes_credentials_in_errors() {
    for input in [
        r#"{"host":"broker","port":1883,"username":"user","password":"secret","unknown":1}"#,
        r#"{"host":"broker","port":65536,"username":"user","password":"secret"}"#,
        r#"{"host":"broker","port":1883,"username":"user","password":"secret\n"}"#,
        r#"{"host":"broker","port":1883,"username":"user"} trailing"#,
        r#"{"host":"broker","host":"two","port":1883,"username":"user"}"#,
        r#"{"host":"broker","port":1883,"username":"user","password":"secret""#,
    ] {
        let error = parse_update(input.as_bytes(), &settings())
            .err()
            .expect("must reject malformed JSON");
        assert!(!error.contains("secret"));
    }
    assert!(parse_update(&[b' '; 4097], &settings()).is_err());
}

#[test]
fn revisions_do_not_lose_updates_or_accept_old_status_callbacks() {
    let control = Control::<NoopRawMutex>::new(settings());
    let mut wait = std::pin::pin!(control.changed(0));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(wait.as_mut().poll(&mut cx).is_pending());
    assert!(!control.apply(settings()));
    assert!(wait.as_mut().poll(&mut cx).is_pending());
    let mut next = settings();
    next.port = 1884;
    assert!(control.apply(next.clone()));
    next.port = 1885;
    assert!(control.apply(next));
    assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(()));
    let state = control.snapshot();
    assert_eq!(state.settings.port, 1885);
    assert_eq!(state.revision, 2);
    control.set_status(0, Status::Connected, None);
    assert_eq!(control.snapshot().status, Status::Connecting);
    control.set_status(2, Status::Disconnected, Some("Connection failed"));
    assert_eq!(control.snapshot().last_error, Some("Connection failed"));
    // A stale pending signal must not wake a supervisor already using revision 2.
    let mut wait = std::pin::pin!(control.changed(2));
    assert!(wait.as_mut().poll(&mut cx).is_pending());
}
