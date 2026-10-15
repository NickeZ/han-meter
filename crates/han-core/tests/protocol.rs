use han_core::{
    Decimal, DecodeError, Decoder, FIELDS, MAX_TELEGRAM_LEN, Value, crc16, parse_telegram,
};

// Frame fixtures use the independent crc crate, rather than the implementation under test.
fn telegram(lines: &str) -> Vec<u8> {
    let body = format!("/TEST5meter\r\n\r\n{lines}!");
    let crc = crc::Crc::<u16>::new(&crc::CRC_16_ARC).checksum(body.as_bytes());
    format!("{body}{crc:04X}\r\n").into_bytes()
}

#[test]
fn checksum_matches_published_check_value() {
    assert_eq!(crc16(b"123456789"), 0xbb3d);
}

#[test]
fn full_swedish_profile_and_precise_energy() {
    let lines = concat!(
        "0-0:1.0.0(261015123456W)\r\n",
        "1-0:1.8.0(12345678.123*kWh)\r\n",
        "1-0:2.8.0(00000.000*kWh)\r\n",
        "1-0:3.8.0(10.100*kVArh)\r\n",
        "1-0:4.8.0(20.200*kVArh)\r\n",
        "1-0:1.7.0(1.111*kW)\r\n",
        "1-0:2.7.0(2.222*kW)\r\n",
        "1-0:3.7.0(3.333*kVAr)\r\n",
        "1-0:4.7.0(4.444*kVAr)\r\n",
        "1-0:21.7.0(5.001*kW)\r\n",
        "1-0:22.7.0(5.002*kW)\r\n",
        "1-0:41.7.0(5.003*kW)\r\n",
        "1-0:42.7.0(5.004*kW)\r\n",
        "1-0:61.7.0(5.005*kW)\r\n",
        "1-0:62.7.0(5.006*kW)\r\n",
        "1-0:23.7.0(6.001*kVAr)\r\n",
        "1-0:24.7.0(6.002*kVAr)\r\n",
        "1-0:43.7.0(6.003*kVAr)\r\n",
        "1-0:44.7.0(6.004*kVAr)\r\n",
        "1-0:63.7.0(6.005*kVAr)\r\n",
        "1-0:64.7.0(6.006*kVAr)\r\n",
        "1-0:32.7.0(230.1*V)\r\n",
        "1-0:52.7.0(231.2*V)\r\n",
        "1-0:72.7.0(232.3*V)\r\n",
        "1-0:31.7.0(1.1*A)\r\n",
        "1-0:51.7.0(2.2*A)\r\n",
        "1-0:71.7.0(3.3*A)\r\n",
        "0-0:96.1.0(meter identifier)\r\n",
    );
    let parsed = parse_telegram(&telegram(lines)).unwrap();
    let expected = [
        "12345678.123",
        "0.000",
        "10.100",
        "20.200",
        "1.111",
        "2.222",
        "3.333",
        "4.444",
        "5.001",
        "5.002",
        "5.003",
        "5.004",
        "5.005",
        "5.006",
        "6.001",
        "6.002",
        "6.003",
        "6.004",
        "6.005",
        "6.006",
        "230.1",
        "231.2",
        "232.3",
        "1.1",
        "2.2",
        "3.3",
        "261015123456W",
    ];
    for (actual, expected) in parsed.readings.iter().zip(expected) {
        assert_eq!(actual.unwrap().to_string(), expected);
    }
}

#[test]
fn every_chunk_boundary_and_back_to_back_frames() {
    let frame = telegram("1-0:1.8.0(10.123*kWh)\r\n");
    for size in 1..=frame.len() {
        let mut decoder = Decoder::new();
        let mut results = Vec::new();
        for chunk in frame.chunks(size) {
            for &byte in chunk {
                if let Some(result) = decoder.push(byte) {
                    results.push(result);
                }
            }
        }
        assert_eq!(results, vec![parse_telegram(&frame)]);
        for &byte in &frame {
            if let Some(result) = decoder.push(byte) {
                results.push(result);
            }
        }
        assert_eq!(results.len(), 2);
    }
}

#[test]
fn noise_truncation_overflow_and_checksum_recovery() {
    let good = telegram("1-0:1.7.0(1.234*kW)\r\n");
    let mut bad = good.clone();
    bad[3] ^= 1;
    let mut decoder = Decoder::new();
    let noise = b"noise\r\n/partial\r\n1-0:1.8.0(";
    for &byte in noise {
        assert!(decoder.push(byte).is_none());
    }
    let results: Vec<_> = bad
        .iter()
        .chain(&good)
        .filter_map(|&b| decoder.push(b))
        .collect();
    assert_eq!(
        results,
        vec![Err(DecodeError::Checksum), parse_telegram(&good)]
    );
    assert!(decoder.push(b'/').is_none());
    let overflow: Vec<_> = (0..MAX_TELEGRAM_LEN + 10)
        .filter_map(|_| decoder.push(b'A'))
        .collect();
    assert_eq!(overflow, vec![Err(DecodeError::Overflow)]);
    let result: Vec<_> = good.iter().filter_map(|&b| decoder.push(b)).collect();
    assert_eq!(result, vec![parse_telegram(&good)]);
}

#[test]
fn invalid_values_and_duplicates_reject_whole_telegram() {
    for line in [
        "1-0:1.8.0(NaN*kWh)",
        "1-0:1.8.0(-1*kWh)",
        "1-0:1.8.0(1*Wh)",
        "1-0:1.8.0(18446744073709551616*kWh)",
        "1-0:1.8.0(1.0000000001*kWh)",
        "1-0:1.8.0(1*kWh)(2*kWh)",
        "0-0:1.0.0(261332123456W)",
        "0-0:1.0.0(260229123456W)",
        "0-0:1.0.0(261015243456W)",
    ] {
        assert_eq!(
            parse_telegram(&telegram(&format!("{line}\r\n"))),
            Err(DecodeError::InvalidValue),
            "{line}"
        );
    }
    assert_eq!(
        parse_telegram(&telegram("1-0:1.8.0(1*kWh)\r\n1-0:1.8.0(2*kWh)\r\n")),
        Err(DecodeError::DuplicateField)
    );
}

#[test]
fn missing_fields_are_absent_and_unknown_fields_are_ignored() {
    let snapshot = parse_telegram(&telegram(
        "9-9:99.99.9(arbitrary)(vendor data)\r\n1-0:1.7.0(0.000*kW)\r\n",
    ))
    .unwrap();
    assert_eq!(snapshot.readings.iter().filter(|v| v.is_some()).count(), 1);
    assert!(snapshot.readings[0].is_none());
    assert_eq!(
        snapshot.readings[4],
        Some(Value::Number(Decimal::parse("0.000").unwrap()))
    );
    assert_eq!(
        parse_telegram(&telegram("9-9:99.99.9(value)\r\n")),
        Err(DecodeError::NoReadings)
    );
}

#[test]
fn invalid_frame_structure_and_checksum_are_not_accepted() {
    let good = telegram("1-0:1.8.0(1*kWh)\r\n");
    for n in 0..good.len() {
        assert!(parse_telegram(&good[..n]).is_err());
    }
    let mut bad = good.clone();
    let len = bad.len();
    bad[len - 3] = b'Z';
    assert_eq!(parse_telegram(&bad), Err(DecodeError::Checksum));
    assert!(parse_telegram(&telegram("1-0:1.8.0(1*kWh)\n")).is_err());
}

#[test]
fn decimals_remain_exact_and_format_without_exponents() {
    for (input, output) in [
        ("00012.001", "12.001"),
        ("0", "0"),
        ("18446744073709551615", "18446744073709551615"),
        ("0.000000001", "0.000000001"),
    ] {
        assert_eq!(Decimal::parse(input).unwrap().to_string(), output);
    }
    for input in ["", ".1", "1.", "1e3", "inf", "1..2", "+1", " 1", "1 "] {
        assert!(Decimal::parse(input).is_err(), "{input}");
    }
}

#[test]
fn field_identifiers_are_unique() {
    for (i, field) in FIELDS.iter().enumerate() {
        for other in &FIELDS[i + 1..] {
            assert_ne!(field.obis, other.obis);
            assert_ne!(field.key, other.key);
        }
    }
}
