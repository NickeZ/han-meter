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
