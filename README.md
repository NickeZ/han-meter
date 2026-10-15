# HAN meter

Bare-metal Rust firmware for an ESP32-C3 that reads Swedish ASCII HAN/P1 telegrams and reports them to Home Assistant using MQTT discovery.

## Hardware and protocol

- P1 data connects directly to **GPIO0**, with an **external 3.3 V pull-up**. The firmware enables input inversion in the GPIO matrix and uses UART1 at **115200 baud, 8N1**.
- The P1 data-request pin is already held high by the hardware. No GPIO output or UART TX is configured for the meter.
- The board is powered from P1 through its regulator. Native USB Serial/JTAG is used for flashing and diagnostic logs.
- The meter's customer port must be activated by the network operator. This firmware expects the Swedish ASCII profile, with `/` framing, OBIS records, and `!` followed by four hexadecimal CRC digits and CRLF. Binary HAN/DLMS telegrams are not supported.

The application runs at 80 MHz and enables minimum Wi-Fi modem sleep. UART reception stays active during network failures. The application uses `no_std`, `esp-hal`, Embassy/`esp-rtos`, `esp-radio`, and `embassy-net`; the ESP-IDF bootloader format is used without the ESP-IDF application runtime. Wi-Fi and MQTT use a fixed 96 KiB allocator arena. Telegram parsing does not allocate.

## Configure, build, and flash

Rustup installs the pinned **Rust 1.94.0** toolchain and `riscv32imc-unknown-none-elf` target when running Cargo in this directory. Install the matching flash tool:

```sh
cargo install espflash --version 4.5.0 --locked
cp config.example.toml config.toml
```

Edit `config.toml` with your device name, 2.4 GHz Wi-Fi credentials, and MQTT broker details. The first version supports WPA2 personal Wi-Fi and MQTT 5 over plain TCP with username/password authentication. The default MQTT port is 1883. Use a DNS hostname or IPv4 address; `.local` mDNS resolution is not implemented.

The firmware sends a DHCP hostname of `han-` followed by the station MAC address as twelve lowercase hexadecimal digits (for example, `han-aabbccddeeff`). The USB startup log shows the name. Routers that display DHCP hostnames can use it in their client list; a cached entry may take time to refresh after reflashing. `device_name` controls the Home Assistant display name. DHCP hostname advertisement does not provide a `.local` mDNS name.

`config.toml` is ignored by Git. Configuration is compiled into the firmware, so changing it requires rebuilding and flashing. Firmware binaries and generated build files also contain those settings. Credentials are never printed in logs or configuration parse errors.

```sh
cargo firmware
cargo flash
```

`cargo flash` builds, flashes, and starts the native USB monitor. If multiple devices are attached, specify the port explicitly:

```sh
espflash board-info --port /dev/ttyACM0
espflash flash --chip esp32c3 --port /dev/ttyACM0 --monitor \
  target/riscv32imc-unknown-none-elf/release/han-meter
```

Use `espflash board-info` to confirm the connected chip and flash capacity. Espflash checks the image against the selected application partition when flashing. No OTA partition scheme or network update service is included.

For compilation without real credentials, use the example configuration explicitly:

```sh
HAN_CONFIG=config.example.toml cargo firmware
```

This produces a build for verification, with placeholder settings. Build again using your real `config.toml` before flashing. `HAN_CONFIG` can also point to an alternative configuration; relative paths are resolved from the repository root. A missing or invalid configuration fails the firmware build. Host tests do not require a configuration file.

## Home Assistant

Enable the MQTT integration and discovery in Home Assistant, using the same MQTT 5 broker. Once a valid telegram arrives, the firmware discovers its supported readings under one device. The device identifier is `han_` followed by the station MAC address as twelve lowercase hexadecimal digits. Renaming the device does not change entity identifiers.

The 26 numeric fields cover:

| Measurements | Directions / phases | Home Assistant units |
| --- | --- | --- |
| Active energy | Total import and export | kWh |
| Reactive energy | Total import and export | kvarh |
| Active power | Total and L1/L2/L3 import and export | kW |
| Reactive power | Total and L1/L2/L3 import and export | kvar |
| Voltage | L1/L2/L3 | V |
| Current | L1/L2/L3 | A |

An additional sensor exposes the meter timestamp as its original `YYMMDDhhmmssW/S` text. No time-zone conversion is applied. Fields absent from a meter's output are not discovered until observed. Unknown OBIS fields are ignored. A malformed known field or invalid checksum rejects the whole telegram.

Energy counters preserve decimal precision and use `total_increasing`; instantaneous readings use `measurement`. Select the **Energy import** and, where relevant, **Energy export** sensors in Home Assistant's Energy dashboard settings. The firmware reports the meter's absolute counters, including legitimate counter resets, without calculating energy from power or inserting zeroes.

| Topic | Purpose | Retained / QoS |
| --- | --- | --- |
| `homeassistant/sensor/<id>_<reading>/config` | Sensor discovery | Yes / 1 |
| `han_meter/<id>/<reading>/state` | Decimal value or raw timestamp | No / 0 |
| `han_meter/<id>/availability` | Device `online` / `offline` | Yes / 1 |
| `han_meter/<id>/<reading>/availability` | Individual reading freshness | Yes / 1 |
| `homeassistant/status` | Subscribed Home Assistant startup notification | Subscription QoS 0 |

Every accepted telegram updates the latest values. Network outages coalesce updates in RAM; no history is written to flash or replayed later. Wi-Fi and MQTT reconnect with exponential delays from 1 to 60 seconds. Discovery and fresh values are replayed after reconnects and Home Assistant startup.

Each reading becomes unavailable after 60 seconds without an update, even when other fields continue arriving. Replayed values retain their original freshness deadline. A retained MQTT Last Will marks the device offline after connection loss. On boot, retained availability is cleared before current data is restored. Discovery messages remain retained across restarts.

## Development and tests

The workspace separates portable behavior from the board:

- `crates/han-core`: bounded streaming decoder, CRC-16/ARC, exact decimal parsing, OBIS mapping, latest-value cache, and discovery JSON.
- `crates/han-mqtt`: the transport-independent MQTT session used by both firmware and broker integration tests.
- `firmware`: GPIO/UART, native USB diagnostics, Wi-Fi/DHCP/DNS/TCP, and build-time configuration.

```sh
cargo test --locked
cargo clippy --locked -p han-core -p han-mqtt --all-targets -- -D warnings
HAN_CONFIG=config.example.toml cargo clippy --locked --release \
  -p han-firmware --target riscv32imc-unknown-none-elf -- -D warnings
cargo fmt --all -- --check
bash scripts/test-mqtt.sh
```

The broker test requires Docker. It starts an isolated Mosquitto container with test credentials on an ephemeral localhost port and removes the container afterward. It exercises the production MQTT session with an independent MQTT observer: discovery, retention, exact values, stale fields, Home Assistant startup, dropped sessions and Last Will, reconnect, empty-cache reboot, and rejected authentication. Unit tests cover malformed and fragmented input, CRC failures, resynchronization, oversized telegrams, invalid dates/units/numbers, missing fields, and counter resets.

## Hardware acceptance

The software tests do not replace these checks on the actual meter and board:

1. Flash with real settings. Confirm the USB startup line reports GPIO0, inversion, 115200 8N1, and the DHCP hostname; verify Wi-Fi and MQTT connection logs and check the router's client list for the hostname.
2. Confirm the `valid` telegram counter rises and compare all reported readings with the meter. The 60-second diagnostic line also reports uptime, invalid frames, UART errors, and meter freshness.
3. Confirm Home Assistant creates a single device, with correct units and Energy dashboard eligibility.
4. Interrupt Wi-Fi, restart the broker and Home Assistant, stop telegram delivery, and power-cycle the board. Verify automatic recovery, stable entity identifiers, unavailable stale readings, and absence of synthetic zeroes.
5. Run for 24 hours using P1 power, including Wi-Fi reconnects. Check for resets, UART errors, missing readings, and supply instability.

If no telegrams arrive, check meter activation and the data-request line. If invalid frames or UART errors rise, check signal inversion, the pull-up/interface wiring, baud rate, and that the meter is using the ASCII profile. Failure to connect to Wi-Fi or MQTT does not stop reception.

Protocol references: [Swedish customer-interface recommendation v2.0](https://www.energiforetagen.se/globalassets/energiforetagen/det-erbjuder-vi/kurser-och-konferenser/elnat/branschrekommendation-lokalt-granssnitt-v2_0-201912.pdf), [Home Assistant MQTT discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery), and [Home Assistant sensor metadata](https://www.home-assistant.io/integrations/sensor/).
