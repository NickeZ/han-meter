use han_core::{Backoff, Decimal, FIELDS, STALE_MS, Snapshot, Telemetry, Value, discovery};

#[test]
fn latest_values_expire_independently() {
    let mut telemetry = Telemetry::new();
    assert!(!telemetry.online(0));
    let mut first = Snapshot::default();
    first.readings[0] = Some(Value::Number(Decimal::parse("12345678.123").unwrap()));
    first.readings[4] = Some(Value::Number(Decimal::parse("1.234").unwrap()));
    telemetry.update(first, 1_000);
    let mut next = Snapshot::default();
    next.readings[4] = Some(Value::Number(Decimal::parse("2.345").unwrap()));
    telemetry.update(next, 50_000);
    assert!(telemetry.online(61_000));
    assert!(!telemetry.readings[0].unwrap().is_fresh(61_000));
    assert!(telemetry.readings[4].unwrap().is_fresh(61_000));
    assert_eq!(telemetry.readings[4].unwrap().generation, 2);
    assert_eq!(telemetry.readings[0].unwrap().generation, 1);
    assert_eq!(telemetry.seen_mask(), 0b10001);
    assert!(!telemetry.online(50_000 + STALE_MS));
}

#[test]
fn recovery_accepts_counter_reset_without_inventing_zeroes() {
    let mut telemetry = Telemetry::new();
    for value in ["100.123", "0.123"] {
        let mut snapshot = Snapshot::default();
        snapshot.readings[0] = Some(Value::Number(Decimal::parse(value).unwrap()));
        telemetry.update(snapshot, 1_000);
    }
    assert_eq!(telemetry.readings[0].unwrap().value.to_string(), "0.123");
    assert!(telemetry.readings[1].is_none());
}

#[test]
fn telegram_age_is_unknown_until_valid_data_and_survives_errors_and_staleness() {
    let mut telemetry = Telemetry::new();
    assert_eq!(telemetry.seconds_since_last_valid(100_000), None);
    let mut snapshot = Snapshot::default();
    snapshot.readings[0] = Some(Value::Number(Decimal::parse("1.234").unwrap()));
    telemetry.update(snapshot, 1_001);
    assert_eq!(telemetry.seconds_since_last_valid(1_000), Some(0));
    assert_eq!(telemetry.seconds_since_last_valid(2_000), Some(0));
    assert_eq!(telemetry.seconds_since_last_valid(2_001), Some(1));
    telemetry.uart_errors += 1;
    telemetry.invalid_frames += 1;
    assert!(!telemetry.online(61_001));
    assert_eq!(telemetry.seconds_since_last_valid(61_001), Some(60));
    telemetry.update(snapshot, 90_000);
    assert_eq!(telemetry.seconds_since_last_valid(90_000), Some(0));
}

#[test]
fn reconnect_backoff_is_bounded_and_resettable() {
    let mut backoff = Backoff::new();
    let delays: Vec<_> = (0..9).map(|_| backoff.next_seconds()).collect();
    assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    backoff.reset();
    assert_eq!(backoff.next_seconds(), 1);
}

#[test]
fn discovery_groups_stable_entities_and_escapes_names() {
    let id = discovery::device_id([0x12, 0x34, 0xab, 0xcd, 0, 0xff]);
    assert_eq!(&id, "han_1234abcd00ff");
    let status = discovery::availability_topic(&id).unwrap();
    let mut buffer = [0; discovery::DISCOVERY_CAPACITY];
    for field in FIELDS {
        let topics = discovery::Topics::new(&id, &field).unwrap();
        let len = discovery::encode(
            &id,
            "Meter \"garage\"\nα",
            "0.1.0",
            &field,
            &topics,
            &status,
            &mut buffer,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&buffer[..len]).unwrap();
        assert_eq!(json["device"]["identifiers"][0], id.as_str());
        assert_eq!(json["device"]["name"], "Meter \"garage\"\nα");
        assert_eq!(json["unique_id"], topics.unique_id.as_str());
        assert_eq!(json["state_topic"], topics.state.as_str());
        assert_eq!(json["availability_mode"], "all");
        assert_eq!(
            json["availability"][1]["topic"],
            topics.availability.as_str()
        );
        assert_eq!(json["expire_after"], 60);
        assert!(json.get("entity_category").is_none());
        if field.key == "energy_import" {
            assert_eq!(json["device_class"], "energy");
            assert_eq!(json["state_class"], "total_increasing");
            assert_eq!(json["unit_of_measurement"], "kWh");
        }
        if field.key == "reactive_power_import" {
            assert_eq!(json["unit_of_measurement"], "kvar");
        }
        if field.key == "meter_timestamp" {
            assert!(json.get("device_class").is_none());
            assert!(json.get("state_class").is_none());
            assert!(json.get("unit_of_measurement").is_none());
        }
        assert!(
            discovery::encode(&id, "Meter", "0.1.0", &field, &topics, &status, &mut [0; 8])
                .is_err()
        );
    }
}

#[test]
fn diagnostics_share_the_device_but_do_not_depend_on_meter_freshness() {
    use discovery::Diagnostic;
    let id = discovery::device_id([0x12, 0x34, 0xab, 0xcd, 0, 0xff]);
    let status = discovery::availability_topic(&id).unwrap();
    let mut buffer = [0; discovery::DISCOVERY_CAPACITY];
    for diagnostic in [Diagnostic::UartErrors, Diagnostic::TelegramAge] {
        let topics = discovery::Topics::for_diagnostic(&id, diagnostic).unwrap();
        let len = discovery::encode_diagnostic(
            &id,
            "Meter \"garage\"\nα",
            "0.1.0",
            diagnostic,
            &topics,
            &status,
            &mut buffer,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&buffer[..len]).unwrap();
        assert_eq!(json["entity_category"], "diagnostic");
        assert_eq!(json["device"]["identifiers"][0], id.as_str());
        assert_eq!(json["device"]["name"], "Meter \"garage\"\nα");
        assert_eq!(json["state_topic"], topics.state.as_str());
        assert_eq!(json["unique_id"], topics.unique_id.as_str());
        assert_eq!(json["availability"].as_array().unwrap().len(), 1);
        assert_eq!(json["availability"][0]["topic"], status.as_str());
        assert_eq!(json["expire_after"], 180);
        match diagnostic {
            Diagnostic::UartErrors => {
                assert_eq!(json["state_class"], "total_increasing");
                assert!(json.get("unit_of_measurement").is_none());
                assert!(json.get("device_class").is_none());
            }
            Diagnostic::TelegramAge => {
                assert_eq!(json["state_class"], "measurement");
                assert_eq!(json["unit_of_measurement"], "s");
                assert_eq!(json["device_class"], "duration");
            }
        }
        assert!(
            discovery::encode_diagnostic(
                &id,
                "Meter",
                "0.1.0",
                diagnostic,
                &topics,
                &status,
                &mut [0; 8],
            )
            .is_err()
        );
    }
}
