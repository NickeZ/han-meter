# HAN meter

Bare-metal Rust firmware for the **Waveshare ESP32-C3-Zero** that reads Swedish ASCII HAN/P1 telegrams and reports them to Home Assistant using MQTT discovery.

## Hardware and protocol

- The board is a [Waveshare ESP32-C3-Zero](https://docs.waveshare.com/ESP32-C3-Zero), based on the ESP32-C3FH4 with 4 MB flash.
- Its onboard WS2812 RGB LED connects to **GPIO10** and uses [RGB byte order](https://docs.waveshare.com/ESP32-C3-Zero/FAQ). It blinks dim green (500 ms on, 500 ms off) while Wi-Fi is disconnected, turns off when the device joins the access point, and resumes blinking after a disconnect. An asynchronous RMT task drives the LED independently of meter reception and MQTT.
- P1 data connects directly to **GPIO0**, with an **external 3.3 V pull-up**. The firmware enables input inversion in the GPIO matrix and uses UART1 at **115200 baud, 8N1**.
- The P1 data-request pin is already held high by the hardware. No GPIO output or UART TX is configured for the meter.
- The board is powered from P1 through its regulator. Native USB Serial/JTAG is used for flashing and diagnostic logs; reception and connection diagnostics are also available from the device's web page while connected to P1.
- The meter's customer port must be activated by the network operator. This firmware expects the Swedish ASCII profile, with `/` framing, OBIS records, and `!` followed by four hexadecimal CRC digits and CRLF. Binary HAN/DLMS telegrams are not supported.

The application runs at 80 MHz and enables minimum Wi-Fi modem sleep. UART reception stays active during network failures. The application uses `no_std`, `esp-hal`, Embassy/`esp-rtos`, `esp-radio`, and `embassy-net`; the ESP-IDF bootloader format is used without the ESP-IDF application runtime. Wi-Fi and MQTT use a fixed 96 KiB allocator arena. Telegram parsing does not allocate.

## Configure, build, and flash

Rustup installs the pinned **Rust 1.94.0** toolchain and `riscv32imc-unknown-none-elf` target when running Cargo in this directory. Install the matching flash tool:

```sh
cargo install espflash --version 4.5.0 --locked
cp config.example.toml config.toml
```

Edit `config.toml` with your device name, 2.4 GHz Wi-Fi credentials, and initial MQTT broker details. The firmware supports WPA2 personal Wi-Fi and MQTT 5 over plain TCP with username/password authentication. The default MQTT port is 1883. Use a DNS hostname or IPv4 address; `.local` mDNS resolution is not implemented.

The firmware sends a DHCP hostname of `han-` followed by the station MAC address as twelve lowercase hexadecimal digits (for example, `han-aabbccddeeff`). The USB startup log shows the name. Routers that display DHCP hostnames can use it in their client list; a cached entry may take time to refresh after reflashing. `device_name` controls the Home Assistant display name. DHCP hostname advertisement does not provide a `.local` mDNS name.

`config.toml` is ignored by Git. Wi-Fi credentials, the device name, and MQTT defaults are compiled into the firmware. Change Wi-Fi credentials or the device name by rebuilding and flashing; MQTT settings can also be changed through the browser after the initial flash. Firmware binaries and generated build files contain the compiled settings. Credentials are never printed in logs or configuration parse errors.

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

## Change MQTT settings over Wi-Fi

After the device joins Wi-Fi, open `http://<device-ip>/` in a browser on the same network. The USB log prints the URL when DHCP assigns an address; you can also find the device in the router's client list using its `han-<MAC>` hostname. The hostname can be used in the browser if your router resolves it. Use the IP address or that advertised hostname; arbitrary DNS aliases and `.local` names are not supported.

Edit the broker host, port, username, and optional new password, then select **Save and reconnect**. The password is never returned to the browser. Leave its field empty to keep the current password. Username/password authentication remains required. Host, username, and password limits are 253, 128, and 256 UTF-8 bytes respectively; control characters are rejected. The port must be 1–65535.

The page shows MQTT connection status and refreshes it every five seconds without overwriting edits. Saving persists the settings before reconnecting MQTT, without rebooting or clearing the latest meter readings. It also interrupts an ongoing connection attempt or retry delay. Incorrect credentials or an unreachable broker remain saved, and the page stays available so you can correct them. The old broker's Last Will marks the previous connection offline; discovery and fresh readings are published on the new connection.

The page uses plain HTTP without a login, intended for a trusted local network. Devices on that network can view the broker address/username and change the settings. Credentials travel over HTTP when replaced. Browser writes must originate from the device's page; the API grants no CORS access.

Saved settings override the compiled MQTT defaults, including after ordinary reflashing. The firmware uses its own checksummed, two-sector journal in the first 8 KiB of the existing `nvs` data partition; it does not use the ESP-IDF NVS format or change the partition layout. Unchanged saves do not write flash. An interrupted update preserves the previous committed record, and blank or corrupt records fall back to the firmware defaults. If the partition is missing, overlaps other data, contains an unsupported format, or cannot be read safely, the firmware uses defaults and disables saving. A flash-write failure keeps the current runtime settings active and disables further writes until restart.

To clear saved MQTT settings or reclaim `nvs` from a previous firmware, exit the USB monitor with **Ctrl+C**, erase that data partition over USB, and restart. For the default layout, the boot log lists `nvs` at offset `0x9000` with length `0x6000`:

```sh
espflash erase-region --chip esp32c3 --port /dev/ttyACM0 0x9000 0x6000
```

Use these addresses only when they match your device's boot log, and replace the port if needed. This erases the entire `nvs` partition. On this firmware it restores the compiled MQTT defaults; Wi-Fi credentials and the application remain in the firmware image. Changing MQTT defaults in `config.toml` alone does not replace a saved override. In espflash 4.5.0, `erase-parts` requires an explicit `--partition-table <FILE>`; it does not read the partition table from the device.

If the page reports **Configuration storage is unavailable**, check the USB startup log for the specific reason. `ForeignData` means the journal contains unrecognized data (for example, ESP-IDF NVS from earlier firmware) or settings from an unsupported version. A valid partition table does not mean that its contents are usable. Ordinary `cargo flash` preserves this data. To discard it, exit the monitor with **Ctrl+C**, run the `erase-region` command above with your device's port, then restart the device and reload the page. The firmware also prints a command using the detected journal address that erases only its two sectors. No full-chip erase is needed. For partition-table, geometry, or read errors, use the reported error to investigate the flash layout or access failure before erasing anything.

The browser uses these local HTTP endpoints:

| Endpoint | Behavior |
| --- | --- |
| `GET /` | Embedded settings page; no external assets or internet connection needed |
| `GET /api/mqtt` | `host`, `port`, `username`, `password_set`, `status`, `last_error`, and `storage_available`; never the password |
| `GET /api/diagnostics?after=<sequence>` | Live reception counters and up to 4 diagnostic events after the supplied sequence (default 0); no credentials |
| `POST /api/mqtt` | JSON containing `host`, `port`, `username`, and optional `password`; omitted, null, or empty password preserves the current one |

API writes require `Content-Type: application/json`, `Content-Length`, and `X-Han-Settings: 1`. Bodies are limited to 4096 bytes, and unknown fields are rejected. A successful response reports `saved`, `changed`, and `message`; saved settings do not imply a successful broker connection. Settings responses are not cached, and the server bounds requests with five-second read/write timeouts.

## Debug without USB

After flashing, connect the device to HAN/P1 and open `http://<device-ip>/`. Select **View diagnostic log**. Diagnostics refresh every two seconds and remain usable when MQTT is disconnected or settings storage is unavailable. **Pause updates** stops browser polling of diagnostics while the device continues recording. **Download log** saves the current counters and the events collected by this browser as `han-meter-debug.txt`.

The page shows uptime, accepted/rejected telegram counts, UART errors split into overflow/framing/glitch/parity/other, the age of the last valid telegram, and the latest and longest intervals between valid telegrams. An unknown interval means fewer than two valid telegrams have arrived. UART errors are receive-error events, not a count of lost telegrams: one telegram can cause multiple errors.

UART1 receives through UHCI0 and GDMA channel 0 into a static 2 KiB ring (eight 256-byte chunks). DMA continues moving bytes while the application is busy; the receive task polls completed chunks every 5 ms and keeps the existing streaming decoder and CRC validation. Length and UART-idle events complete chunks, including the short tail of a telegram. An empty ring holds up to about 178 ms of continuous 115200 8N1 traffic; short idle-terminated chunks use a whole descriptor, so fragmented input reduces that headroom. The startup log includes `UART DMA active` and `RX FIFO threshold=32`.

DMA checks ownership before reusing a chunk, so it stops instead of overwriting unread data. On UART or DMA faults, the firmware stops the channel, discards queued bytes and the partial telegram, resets reception, and resumes at the next valid telegram. The web page and downloaded log show DMA bytes delivered to the decoder, DMA errors, and confirmed full-ring overruns separately from UART errors. `FifoOverflowed` still means the UART hardware FIFO filled before DMA drained it. A sufficiently long delay can exhaust both buffers; both error categories are recorded when their flags are present. No finite buffer guarantees loss-free reception during an unbounded stall.

Events include startup/storage messages, Wi-Fi and MQTT connection/retry events, UART error types, telegram rejection reasons, and valid-telegram gaps of at least 1.5 seconds. Gap events measure arrival timing, so meters with a slower normal cadence also produce them. Existing application USB messages are mirrored; UART and parser errors are recorded only in RAM to avoid synchronous USB writes in the receive task. This is an application log, not a capture of bootloader output or panic backtraces. No credentials, HTTP request bodies, or raw meter telegrams are logged.

The device retains its latest 64 events with messages limited to 160 bytes, overwriting the oldest entries. It does not write logs to flash; rebooting clears the log and counters. UART messages are limited to one per second per error type, DMA messages to one per second, and parser rejection messages to one per second, while all error counters continue increasing. The browser keeps up to 256 lines and reports when older device entries were overwritten or a restart is detected. Download before closing the page or restarting if you need to retain the evidence.

The diagnostic API uses the same local Host checks as the settings API, returns `Cache-Control: no-store`, and includes a `boot_id`, millisecond timestamps, counters, and a `log` object. Use `log.next_after` as the next cursor; `log.more` indicates another page and `log.lost` indicates overwritten events. Reset the cursor when `boot_id` changes. Log formatting and serialization run outside the log's critical sections, and HTTP snapshots copy one bounded entry at a time.

The settings and diagnostic JSON responses use bounded heap allocations to keep large values out of nested HTTP futures. HTTP tests enforce a future-size budget, and the firmware linker requires at least 80 KiB of main stack headroom. These checks catch memory growth that a successful host HTTP test alone would miss.

## Home Assistant

Enable the MQTT integration and discovery in Home Assistant, using the same MQTT 5 broker. The firmware discovers two diagnostic sensors on connection and adds supported meter readings once a valid telegram arrives, all under one device. The device identifier is `han_` followed by the station MAC address as twelve lowercase hexadecimal digits. Renaming the device does not change entity identifiers.

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

Two sensors appear in the device's **Diagnostics** section:

- **UART error count**: UART receive errors since boot, resetting to zero after a restart. Telegram parse/CRC failures are counted separately in the USB log.
- **Seconds since last valid telegram**: elapsed whole seconds since the most recent accepted telegram. It is unknown until the first valid telegram; invalid input does not reset the age.

Diagnostics publish every 60 seconds, immediately after MQTT connection, and when Home Assistant announces startup. They remain available while MQTT is connected, including when the meter is silent or its data has expired. Diagnostic discovery is retained; diagnostic values are not retained and expire in Home Assistant after 180 seconds without an update.

| Topic | Purpose | Retained / QoS |
| --- | --- | --- |
| `homeassistant/sensor/<id>_<reading>/config` | Sensor discovery | Yes / 1 |
| `han_meter/<id>/<reading>/state` | Decimal value or raw timestamp | No / 0 |
| `han_meter/<id>/uart_errors/state` | UART error count since boot | No / 0 |
| `han_meter/<id>/seconds_since_last_valid_telegram/state` | Whole seconds, or `None` before the first valid telegram | No / 0 |
| `han_meter/<id>/availability` | MQTT connection `online` / `offline` | Yes / 1 |
| `han_meter/<id>/<reading>/availability` | Individual reading freshness | Yes / 1 |
| `homeassistant/status` | Subscribed Home Assistant startup notification | Subscription QoS 0 |

Every accepted telegram updates the latest values. Network outages coalesce updates in RAM; no history is written to flash or replayed later. Wi-Fi and MQTT reconnect with exponential delays from 1 to 60 seconds. Discovery and fresh values are replayed after reconnects and Home Assistant startup.

Each meter reading becomes unavailable after 60 seconds without an update, even when other fields continue arriving. Replayed values retain their original freshness deadline. A retained MQTT Last Will marks the device and its diagnostics offline after connection loss. On boot, retained availability is cleared before current data is restored. Discovery messages remain retained across restarts.

## Development and tests

The workspace separates portable behavior from the board:

- `crates/han-core`: bounded streaming decoder, CRC-16/ARC, exact decimal parsing, OBIS mapping, latest-value cache, and discovery JSON.
- `crates/han-mqtt`: the transport-independent MQTT session used by both firmware and broker integration tests.
- `crates/han-config`: bounded settings and shared validation, persistent journal, change notifications, and HTTP handlers.
- `firmware`: GPIO/UART, native USB diagnostics, Wi-Fi/DHCP/DNS/TCP, flash and HTTP tasks, and build-time defaults.

```sh
cargo test --locked
cargo clippy --locked -p han-core -p han-mqtt -p han-config --all-targets -- -D warnings
HAN_CONFIG=config.example.toml cargo clippy --locked --release \
  -p han-firmware --target riscv32imc-unknown-none-elf -- -D warnings
cargo fmt --all -- --check
node scripts/test-web-ui.cjs
bash scripts/test-mqtt.sh
```

The broker tests require Docker. They start two isolated Mosquitto containers with test credentials on ephemeral localhost ports and remove them afterward. They exercise the production MQTT session with independent MQTT observers: discovery, retention, exact values, stale fields, periodic diagnostics with and without meter data, Home Assistant startup, dropped sessions and Last Will, reconnect, empty-cache reboot, rejected authentication, live broker/credential changes, and corrections during retry backoff. The host tests also cover malformed/fragmented HTTP requests, password preservation, storage errors, settings validation, revision notifications, and simulated power cuts throughout flash erase/write/verification. Protocol tests cover malformed and fragmented input, CRC failures, resynchronization, oversized telegrams, invalid dates/units/numbers, missing fields, counter resets, and diagnostic metadata/telegram age. DMA ring tests run the firmware's actual buffer code with a simulated producer, covering delayed consumption, ownership, wraparound, short/empty chunks, invalid lengths, reset, and telegram decoding across chunks. They do not emulate the UART/UHCI/GDMA peripherals; hardware acceptance remains necessary.

## Hardware acceptance

The software tests do not replace these checks on the actual meter and board:

1. Flash with real settings. Confirm the USB startup line reports GPIO0, inversion, 115200 8N1, and the DHCP hostname; verify Wi-Fi and MQTT connection logs and check the router's client list for the hostname. Verify the onboard LED blinks green until Wi-Fi connects, then turns off.
2. Confirm the `valid` telegram counter rises and compare all reported readings with the meter. The 60-second diagnostic line also reports uptime, invalid frames, UART errors, and meter freshness.
3. Confirm Home Assistant creates a single device, with correct units and Energy dashboard eligibility. Verify its two diagnostic sensors update every minute and the telegram age increases when P1 input stops while MQTT stays connected.
4. Interrupt Wi-Fi, restart the broker and Home Assistant, stop telegram delivery, and power-cycle the board. Verify automatic recovery, stable entity identifiers, unavailable stale readings, and absence of synthetic zeroes. Check that the green blink resumes during Wi-Fi outages and stops after reconnection, including when MQTT is unavailable.
5. Run for 24 hours using P1 power, including Wi-Fi reconnects. Check for resets, UART errors, missing readings, and supply instability.
6. Open the settings page, change broker/port/credentials, and verify that the old broker becomes offline and the new broker receives discovery and readings. Save an incorrect password, correct it during retry backoff, and confirm recovery without a reboot. Interrupt Wi-Fi and verify that the page returns when it reconnects.
7. Power-cycle after saving and after an interrupted save; verify that the previous or new complete settings are loaded. Reflash normally and confirm the override survives. Check UART diagnostics during repeated saves: synchronous flash operations can briefly delay UART servicing, so verify that reception recovers and valid telegrams continue.
8. With USB disconnected and HAN/P1 connected, open the diagnostic log and confirm `UART DMA active`. Verify that DMA bytes and valid telegram counts advance, pause/resume works, and the downloaded text includes the counters and events. Compare UART and DMA error growth with the page open and closed; reconnect the network and restart the device to check log recovery and reset detection. Repeatedly save settings and verify reception resumes after any logged DMA recovery. Unplug/reconnect the meter input and verify that short final chunks and subsequent telegrams are delivered without waiting for another telegram.

If no telegrams arrive, check meter activation and the data-request line. If invalid frames or UART errors rise, check signal inversion, the pull-up/interface wiring, baud rate, and that the meter is using the ASCII profile. Failure to connect to Wi-Fi or MQTT does not stop reception.

Protocol references: [Swedish customer-interface recommendation v2.0](https://www.energiforetagen.se/globalassets/energiforetagen/det-erbjuder-vi/kurser-och-konferenser/elnat/branschrekommendation-lokalt-granssnitt-v2_0-201912.pdf), [Home Assistant MQTT discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery), and [Home Assistant sensor metadata](https://www.home-assistant.io/integrations/sensor/).
