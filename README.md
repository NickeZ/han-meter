# HAN meter

Bare-metal Rust firmware for the **Waveshare ESP32-C3-Zero** that reads Swedish ASCII HAN/P1 telegrams and reports them to Home Assistant using MQTT discovery.

## Hardware and protocol

The board is a [Waveshare ESP32-C3-Zero](https://docs.waveshare.com/ESP32-C3-Zero), based on the ESP32-C3FH4 with 4 MB flash.

### Passive splitter and Easee Equalizer

The current installation shares the meter's HAN/P1 port between an **Easee Equalizer** and this board using a **passive port splitter**. The Equalizer holds the P1 **data-request line high** so the meter sends telegrams, and supplies the **pull-up on the shared data line connected to UART RX**. There is no separate RX pull-up resistor on the ESP32 branch in this setup.

The ESP32 only listens to the shared data line. The firmware configures neither a data-request GPIO output nor a UART TX pin for the meter. The UART driver enables a weak internal RX pull-up, but this wiring relies on the Equalizer's external pull-up. The passive splitter only joins the connections; it does not generate a request signal, provide a pull-up, buffer the data, or translate voltage levels.

Keep the Equalizer connected and powered for this arrangement to work. Without it, the request signal and RX pull-up must be supplied by suitable external hardware. A standalone installation needs both a data-request circuit meeting the meter's requirements and an external pull-up/interface that presents a **3.3 V-compatible signal** to GPIO0; the firmware does not provide either function.

### Wiring and requirements

| Connection | Requirement |
| --- | --- |
| P1 data | Connect to **GPIO0**. The firmware uses UART1 at **115200 baud, 8N1**, with input inversion in the GPIO matrix. The board pad labelled `RX` is GPIO20, not the input used by this firmware. |
| P1 data request | Held high by the Equalizer through the splitter; no connection to an ESP32 output is needed. |
| Ground | Connect the meter's signal ground and power return to board **GND** as specified by the meter's pinout, providing a common reference for the shared data line. |
| P1 power | Feed the meter's **5 V** supply to the board's **5V** pad, through its onboard 3.3 V regulator. Do not feed 5 V into the **3V3** pad or GPIO0. |
| USB | Use a USB-C **data cable** for flashing and native USB Serial/JTAG logs. USB is not required during normal P1-powered operation. |

- **Meter compatibility:** the customer port must be activated by the network operator. This firmware expects the Swedish ASCII profile, with `/` framing, OBIS records, and `!` followed by four hexadecimal CRC digits and CRLF. Binary HAN/DLMS telegrams are not supported; a port labelled HAN is not sufficient to establish compatibility.
- **Splitter and cables:** use a P1-compatible passive splitter and cables that preserve the required power, ground, data, and data-request connections to both branches. Check the meter's connector pinout and cable continuity rather than relying on wire colours or the connector's appearance.
- **RX voltage:** verify the shared data line's high level before connecting GPIO0. The Equalizer supplies the pull-up in this installation, but its presence alone does not establish voltage compatibility for other models or wiring. ESP32-C3 GPIOs use 3.3 V logic and must not be driven by a 5 V pull-up; use a suitable level interface if necessary. Firmware inversion changes signal polarity, not voltage. See the [ESP32-C3 electrical characteristics](https://documentation.espressif.com/esp32-c3_datasheet_en.html).
- **Power budget:** the meter must supply enough current for the Equalizer and ESP32 together, including Wi-Fi transmit and reconnect peaks. A passive splitter adds no power capacity. P1 power availability and limits depend on the meter; verify them in its documentation. If an external supply is needed, isolate its supply feed from the meter's power output to avoid backfeeding. Check the power paths before connecting USB and P1 power simultaneously.
- **Network coverage:** provide a usable 2.4 GHz WPA2 personal Wi-Fi signal at the meter and access to the MQTT broker. Check reception with the meter cabinet closed.

The board's power input, GPIO voltage, and pin labels are documented in the [Waveshare hardware description](https://docs.waveshare.com/ESP32-C3-Zero#hardware-description). Reception and connection diagnostics are also available from the device's web page while connected to P1.

The onboard WS2812 RGB LED connects to **GPIO10** and uses [RGB byte order](https://docs.waveshare.com/ESP32-C3-Zero/FAQ). It blinks dim green (500 ms on, 500 ms off) while Wi-Fi is disconnected, turns off when the device joins the access point, and resumes blinking after a disconnect. An asynchronous RMT task drives the LED independently of meter reception and MQTT.

The application runs at 80 MHz and enables minimum Wi-Fi modem sleep. UART reception stays active during network failures. The application uses `no_std`, `esp-hal`, Embassy/`esp-rtos`, `esp-radio`, and `embassy-net`; the ESP-IDF bootloader format is used without the ESP-IDF application runtime. Wi-Fi, MQTT, HTTP, and TLS share a 160 KiB heap: 96 KiB in the main RAM region plus 64 KiB reclaimed from the finished ESP-IDF bootloader. The extra region does not reduce the main stack reserve. The dedicated MQTT/network RTOS thread reserves a separate 32 KiB stack from that heap before Wi-Fi initialization, whether or not HTTPS logging is enabled. Telegram parsing does not allocate.

UART reception and MQTT delivery take precedence over every secondary application feature:

| Work | Execution priority |
| --- | --- |
| UART DMA reception and telegram decoding | Dedicated interrupt executor, above application threads |
| MQTT, Wi-Fi management, and the network stack | Dedicated RTOS thread, priority 1 |
| Settings/diagnostic HTTP handlers, USB logs, status LED, periodic diagnostics, and HTTPS log delivery | Main RTOS thread, priority 0 |

Ready MQTT/network work preempts background work, including synchronous TLS cryptography and HTTP response formatting. Secondary features may be delayed or starved under load. Socket operations remain on the network thread because `embassy-net` is not thread-safe; HTTP and HTTPS workers exchange bounded 256-byte messages with socket owners there. No HTTP parsing, JSON serialization, TLS cryptography, or USB writes run on that thread. Brief critical sections and synchronous flash operations during settings saves can still delay servicing; thread priority cannot preempt interrupt masking or flash/cache stalls.

## Configure, build, and flash

Rustup installs the pinned **Rust 1.95.0** toolchain and `riscv32imc-unknown-none-elf` target when running Cargo in this directory. The ESP dependencies require Rust 1.95 or newer. Install the matching flash tool:

```sh
cargo install espflash --version 4.5.0 --locked
cp config.example.toml config.toml
```

The HTTPS client uses Rust-native `embedded-tls` and RustCrypto. Cargo fetches pinned Git revisions of `embedded-tls` and its Embassy crypto drivers; no C TLS toolchain is required.

Edit `config.toml` with your device name, 2.4 GHz Wi-Fi credentials, and initial MQTT broker details. The firmware supports WPA2 personal Wi-Fi and MQTT 5 over plain TCP with username/password authentication. The default MQTT port is 1883. Use a DNS hostname, IPv4 address, or IPv6 address; `.local` mDNS resolution is not implemented. IPv6 literals may be bare (`fd12:3456:789a::10`) or bracketed (`[fd12:3456:789a::10]`). Keep the port in its separate field and omit zone IDs such as `%wlan0`; link-local broker addresses use the device's Wi-Fi interface.

IPv4 uses DHCP and IPv6 uses SLAAC router advertisements, with both enabled together. For hostnames, MQTT tries AAAA records first when IPv6 is configured, then falls back to A records when IPv4 is configured. It tries up to four addresses per family, with a five-second TCP connection timeout per address. An IPv4-only network does not wait for IPv6 configuration.

IPv6 router discovery retries unanswered requests with exponential backoff and jitter, starting at about four seconds and backing off to about an hour. Losing the initial requests does not require a reboot; retries stop when a router advertises a default route. The local [smoltcp patches](vendor/smoltcp/PATCHES.md) also preserve discovery timers when network sockets are idle.

On an IPv6-only network, use a literal broker address. The pinned network stack does not learn DNS servers from IPv6 router advertisements (RDNSS) or implement DHCPv6; hostname lookup relies on DNS servers supplied by DHCPv4. A global or ULA IPv6 address requires a router advertising a SLAAC prefix. Link-local broker literals also work on the same Wi-Fi link without DHCP or a router.

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

After the device joins Wi-Fi, open `http://<device-ip>/` in a browser on the same network, or `http://[<device-ipv6>]/` for IPv6. The USB log prints URLs when DHCP or SLAAC assigns an address; you can also find the device in the router's client list using its `han-<MAC>` hostname. The hostname can be used in the browser if your router resolves it. Use the IP address or that advertised hostname; arbitrary DNS aliases and `.local` names are not supported.

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

UART1 receives through UHCI0 and GDMA channel 0 into a static 2 KiB ring (eight 256-byte chunks). DMA continues moving bytes while the application is busy; the receive task polls completed chunks every 5 ms on a dedicated interrupt executor (`FROM_CPU_INTR1`, priority 1), so it can preempt both MQTT/network work and background log delivery. Reception and parsing do not allocate or write to USB, and each poll handles at most eight chunks. Length and UART-idle events complete chunks, including the short tail of a telegram. An empty ring holds up to about 178 ms of continuous 115200 8N1 traffic; short idle-terminated chunks use a whole descriptor, so fragmented input reduces that headroom. The startup log includes `UART DMA active`, `RX FIFO threshold=32`, and `interrupt executor, poll=5 ms`.

DMA checks ownership before reusing a chunk, so it stops instead of overwriting unread data. On UART or DMA faults, the firmware stops the channel, discards queued bytes and the partial telegram, resets reception, and resumes at the next valid telegram. The web page and downloaded log show DMA bytes delivered to the decoder, DMA errors, and confirmed full-ring overruns separately from UART errors. `FifoOverflowed` still means the UART hardware FIFO filled before DMA drained it. A sufficiently long delay can exhaust both buffers; both error categories are recorded when their flags are present. No finite buffer guarantees loss-free reception during an unbounded stall.

`UART DMA poll delayed` reports intervals of at least 50 ms between receive polls, limited to one message per second. UART/DMA error events also include `poll_gap` in milliseconds. These help distinguish delayed servicing from peripheral or signal faults. Long interrupt-disabled sections can still delay reception. In older firmware, reception shared the main executor with TLS: a fresh log-upload handshake could fill the DMA ring, overflow the UART FIFO, and cause several seconds between valid telegrams. The resulting error logs then triggered more uploads.

Events include startup/storage messages, Wi-Fi and MQTT connection/retry events, UART error types, telegram rejection reasons, and valid-telegram gaps of at least 1.5 seconds. Gap events measure arrival timing, so meters with a slower normal cadence also produce them. All application messages are recorded in RAM. A background task drains this log to USB asynchronously, with a 20 ms deadline per entry and no synchronous USB writes in UART or MQTT tasks. Slow or absent USB monitors may miss or truncate entries; the RAM log remains available through HTTP and HTTPS export. This is an application log, not a capture of bootloader output or panic backtraces. No credentials, HTTP request bodies, or raw meter telegrams are logged.

The device retains its latest 64 events with messages limited to 160 bytes, overwriting the oldest entries. It does not write logs to flash; rebooting clears the log and counters. UART messages are limited to one per second per error type, DMA messages to one per second, and parser rejection messages to one per second, while all error counters continue increasing. The browser keeps up to 256 lines and reports when older device entries were overwritten or a restart is detected. Download before closing the page or restarting if you need to retain the evidence.

The diagnostic API uses the same local Host checks as the settings API, returns `Cache-Control: no-store`, and includes a `boot_id`, millisecond timestamps, counters, and a `log` object. Use `log.next_after` as the next cursor; `log.more` indicates another page and `log.lost` indicates overwritten events. Reset the cursor when `boot_id` changes. Log formatting and serialization run outside the log's critical sections, and HTTP snapshots copy one bounded entry at a time.

The settings and diagnostic JSON responses use bounded heap allocations to keep large values out of nested HTTP futures. HTTP's request and TCP buffers also use fallible heap allocations (10 KiB total); allocation failure disables the HTTP server without stopping UART/MQTT. HTTP tests enforce a future-size budget, and the firmware linker requires at least 80 KiB of main stack headroom for HTTP/TLS and interrupts, separate from the MQTT thread's stack. These checks catch memory growth that a successful host HTTP test alone would miss.

Heap diagnostics are logged at startup, after Wi-Fi initialization, after allocating remote-log buffers, and every minute. `free` is the current free heap and `min_free` is the lowest estimated free heap since boot, both in bytes. These include transient allocation peaks such as TLS certificate verification; total free bytes do not guarantee a contiguous block of that size.

## Send logs to VictoriaLogs

Add this optional table to `config.toml`, then rebuild and flash:

```toml
[victorialogs]
url = "https://logs.example.net/insert/jsonline"
username = "han-meter"
password = "CHANGE_ME"
# ntp_server = "pool.ntp.org"
```

Use the complete [VictoriaLogs JSON stream ingest URL](https://docs.victoriametrics.com/victorialogs/data-ingestion/#json-stream-api), including any reverse-proxy prefix or tenant query parameters. The firmware posts newline-delimited JSON with `Content-Type: application/stream+json` and HTTP Basic authentication. Credentials are compiled into the image, like Wi-Fi credentials; they are not exposed through the settings page. Omitting the table disables delivery. Changing these settings requires rebuilding and flashing.

HTTPS certificate verification is mandatory: the firmware checks the hostname, certificate chain, and validity dates before sending credentials. The included [Let's Encrypt ISRG Root X1 and X2 certificates](firmware/certs/README.md) support ordinary certificate renewals without reflashing. The server must send its intermediate certificates and support TLS 1.3 with AES-128-GCM/SHA-256 and P-256 or P-384 key exchange. RSA and ECDSA certificates are supported, including Let's Encrypt's P-384/SHA-384 intermediates. The upstream verifier limits hostnames to 64 bytes and our leaf-certificate buffer holds 4096 DER bytes. Self-signed certificates, unrelated CAs, plain HTTP, TLS 1.2-only servers, and redirects are rejected.

The TLS dependency is pinned to upstream `main` revision [`6487c62`](https://github.com/embassy-rs/embedded-tls/commit/6487c62bb084c33361822022e21836ddfd48388f), which includes WebPKI chain verification unavailable in the published 0.19.0 adapter. Our wrapper tries both roots and refuses missing time, including if time becomes unavailable during the handshake; it never enables upstream's optional date-check bypass. TLS records and certificate storage use reusable heap buffers with fallible startup allocation. Log batching, TLS cryptography, and retry handling run on the main background thread at priority 0 alongside HTTP, diagnostics, and the LED. MQTT and the network executor run on their own thread at priority 1, and UART reception runs in interrupt context. Ready MQTT/network work therefore preempts uploads even inside a synchronous TLS handshake; UART can preempt both threads. Busy foreground work may delay or starve log delivery, which is intentional.

The uploader exchanges at most 256 bytes per request with a socket-owner task on the MQTT/network executor. All TCP/DNS/SNTP access remains on that thread; the non-thread-safe network stack is never shared with the background thread. The socket owner performs only bounded copies and asynchronous I/O. After a timeout, closing cancels pending socket I/O and discards queued bytes before acknowledging closure and allowing a fresh TLS session. HTTP uses independent read/write bridges so a stalled request read cannot block a response. Upload diagnostics enter the same RAM log and background USB output as other events. The enable event reports `background priority=0, MQTT priority=1`.

The device synchronizes its clock over SNTP before uploading and refreshes it every six hours. Permit UDP port 123 to `ntp_server`, or set a reachable local NTP hostname or IP address (bracket IPv6 literals). Uploads wait while synchronization fails. SNTP is unauthenticated; use a trusted network/time server. DNS has the same DHCPv4 dependency described above for MQTT.

The local log reports `NTP synchronized` when time is available and `VictoriaLogs first log batch delivered` after the first successful upload. NTP failures distinguish DNS errors/timeouts, unanswered UDP requests, and rejected responses; later TLS/HTTP failures are reported independently. SNTP alternates IPv6 and IPv4 candidates with five-second DNS and three-second per-address timeouts, trying all returned addresses before retrying.

Every retained application diagnostic event is exported, including events recorded only in RAM to avoid blocking UART reception. Each line contains `_msg`, `_time` (Unix milliseconds), `device` (the DHCP hostname), `boot_id`, `firmware`, `sequence`, and `uptime_ms`. VictoriaLogs groups streams by `device`. Timestamps are estimated from the first clock synchronization and monotonic uptime; `boot_id` and `sequence` identify events within a boot.

Startup, Wi-Fi, and periodic status/heap messages are uploaded even when no HAN telegrams arrive. Delivery starts as soon as time synchronization and a verified HTTPS connection succeed.

The uploader checks for new events every five seconds and drains a backlog in batches of up to four. It retains each batch until a successful HTTP response, retrying failures with delays from 1 to 60 seconds and a fresh TLS connection. An acknowledgement lost after ingestion can produce duplicates with the same event identifiers. Network operations have timeouts and upload errors are logged only when entering or recovering from a failure.

Delivery uses the existing 64-event RAM log and its 160-byte message limit and rate limits. Older entries can be overwritten during an outage; `lost_entries` on the next batch reports the gap. Pending logs disappear at reset, and bootloader output and panic backtraces are not captured. Logs are never written to flash. This preserves useful history remotely while the device runs, but cannot guarantee delivery of the final event before a crash.

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
- `crates/han-logs`: bounded JSON Lines batches, HTTPS/Basic-auth validation, HTTP upload, and SNTP response validation.
- `firmware`: GPIO/UART, native USB diagnostics, Wi-Fi/DHCP/SLAAC/DNS/TCP, flash and HTTP tasks, and build-time defaults.

```sh
cargo test --locked
cargo test --locked -p han-logs --features tls
cargo clippy --locked -p han-core -p han-mqtt -p han-config -p han-logs \
  --features han-logs/tls --all-targets -- -D warnings
HAN_CONFIG=config.example.toml cargo clippy --locked --release \
  -p han-firmware --target riscv32imc-unknown-none-elf -- -D warnings
cargo fmt --all -- --check
node scripts/test-web-ui.cjs
bash scripts/test-mqtt.sh
```

The broker tests require Docker. They start two isolated Mosquitto containers with test credentials on ephemeral localhost ports and remove them afterward. They exercise the production MQTT session with independent MQTT observers: discovery, retention, exact values, stale fields, periodic diagnostics with and without meter data, Home Assistant startup, dropped sessions and Last Will, reconnect, empty-cache reboot, rejected authentication, live broker/credential changes, and corrections during retry backoff. The host tests also cover malformed/fragmented HTTP requests, password preservation, storage errors, settings validation, revision notifications, and simulated power cuts throughout flash erase/write/verification. Protocol tests cover malformed and fragmented input, CRC failures, resynchronization, oversized telegrams, invalid dates/units/numbers, missing fields, counter resets, and diagnostic metadata/telegram age. DMA ring tests run the firmware's actual buffer code with a simulated producer, covering delayed consumption, ownership, wraparound, short/empty chunks, invalid lengths, reset, and telegram decoding across chunks. They do not emulate the UART/UHCI/GDMA peripherals; hardware acceptance remains necessary.

The byte-stream bridge tests cover fragmented I/O, socket errors, EOF, late replies after cancellation, connection resets, and operation across threads without moving the socket. HTTP bridge tests run the firmware's actual socket adapter and cover fragmented requests/responses, concurrent reads/writes, read/write timeouts, and subsequent connection reuse. They do not emulate RTOS priorities.

The HTTPS integration test requires Node.js and OpenSSL. It generates a temporary CA/intermediate and RSA/ECDSA server certificates, starts an independent local HTTPS server, and checks real TLS 1.3 through the same bounded byte-stream bridge used by the firmware, Basic authentication, certificate/date/hostname rejection, missing intermediates, trust-root fallback, clock loss during verification, HTTP errors, redirects, and cancellation/reconnection. Protocol tests cover JSON escaping, bounded batches, overwritten events, fragmented HTTP, and SNTP validation/era rollover. No test uses production credentials or contacts a production log endpoint.

Host tests also exercise the firmware's broker address selection with simulated DNS/TCP failures: IPv6 preference, IPv4 fallback, multiple answers, literal addresses, and operation with only one configured address family. IPv6 settings validation, journal persistence, and HTTP Host/Origin handling are covered separately. A packet-level SLAAC regression test injects a router advertisement after IPv4 is configured and verifies that both gateways and all three addresses coexist. These tests do not emulate the Wi-Fi network.

`vendor/smoltcp` contains the pinned 0.13.1 source with a one-line fix for SLAAC dropping the IPv6 gateway when an IPv4 route already exists. See [the patch notes](vendor/smoltcp/PATCHES.md) for provenance, the upstream report, and removal criteria.

The ESP crates are pinned together at compatible releases: `esp-hal` 1.2.2, `esp-rtos` 0.4.0, and `esp-radio` 1.0.0-beta.1. These include fixes for restoring the RISC-V idle context, cancelling a Wi-Fi connection in progress, and returning driver state/timeout errors without panicking. Wi-Fi is configured once; failed or timed-out connection attempts are explicitly disconnected before retrying with backoff.

## Hardware acceptance

The software tests do not replace these checks on the actual meter and board:

- On a dual-stack LAN, verify both DHCP and SLAAC URLs appear, connect to an IPv6 broker literal and an AAAA-only hostname, and save settings through the IPv6 URL. Check fallback with an unreachable AAAA address and a reachable A address. Repeat on IPv4-only Wi-Fi.
- Disable DHCPv4 and verify an IPv6 broker literal works using SLAAC. Test a link-local broker literal on the same link without router advertisements, then reconnect Wi-Fi and confirm recovery.

1. Flash with real settings. Confirm the USB startup line reports GPIO0, inversion, 115200 8N1, and the DHCP hostname; verify Wi-Fi and MQTT connection logs and check the router's client list for the hostname. Verify the onboard LED blinks green until Wi-Fi connects, then turns off.
2. Confirm the `valid` telegram counter rises and compare all reported readings with the meter. The 60-second diagnostic line also reports uptime, invalid frames, UART errors, and meter freshness.
3. Confirm Home Assistant creates a single device, with correct units and Energy dashboard eligibility. Verify its two diagnostic sensors update every minute and the telegram age increases when P1 input stops while MQTT stays connected.
4. Interrupt Wi-Fi, restart the broker and Home Assistant, stop telegram delivery, and power-cycle the board. Verify automatic recovery, stable entity identifiers, unavailable stale readings, and absence of synthetic zeroes. Check that the green blink resumes during Wi-Fi outages and stops after reconnection, including when MQTT is unavailable.
5. Run for at least 72 hours using P1 power with the Equalizer connected through the passive splitter, including quiet periods and repeated Wi-Fi outages longer than the 30-second connection timeout. Restore the access point and verify automatic recovery without resets. Check for UART errors, missing readings, and supply instability on both devices.
6. Open the settings page, change broker/port/credentials, and verify that the old broker becomes offline and the new broker receives discovery and readings. Save an incorrect password, correct it during retry backoff, and confirm recovery without a reboot. Interrupt Wi-Fi and verify that the page returns when it reconnects.
7. Power-cycle after saving and after an interrupted save; verify that the previous or new complete settings are loaded. Reflash normally and confirm the override survives. Check UART diagnostics during repeated saves: synchronous flash operations can briefly delay UART servicing, so verify that reception recovers and valid telegrams continue.
8. With USB disconnected and HAN/P1 connected, open the diagnostic log and confirm `UART DMA active`. Verify that DMA bytes and valid telegram counts advance, pause/resume works, and the downloaded text includes the counters and events. Compare UART and DMA error growth with the page open and closed; reconnect the network and restart the device to check log recovery and reset detection. Repeatedly save settings and verify reception resumes after any logged DMA recovery. Unplug/reconnect the meter input and verify that short final chunks and subsequent telegrams are delivered without waiting for another telegram.
9. With VictoriaLogs enabled, verify startup and reconnect events arrive with the correct device, time, and boot ID. Confirm the UART startup event includes `interrupt executor, poll=5 ms`. During repeated uploads, verify valid telegrams keep their normal cadence and UART/DMA error counters do not increase. If errors remain, retain the `poll_gap` values and any `UART DMA poll delayed` events. Compare with a build that omits `[victorialogs]` to isolate upload-related stalls. Interrupt HTTPS/NTP access, return 401/429/503 responses, and restore service; check recovery and reported log gaps. Exercise uploads together with MQTT reconnects and the settings page during the 72-hour run, checking for resets, UART/DMA overruns, and missing telegrams. Confirm `background priority=0, MQTT priority=1` appears in the diagnostic log. Observe MQTT arrival times with an independent broker subscriber during successful and repeatedly failing TLS handshakes: meter readings, MQTT keepalives, and reconnects must continue while uploads run. Check heap low-water marks and stack guards with the actual certificate chain; the MQTT/network thread uses a 32 KiB heap-backed stack. Host tests cannot establish ESP32 scheduling latency or memory headroom.

If no telegrams arrive, check meter activation, splitter/cable continuity, and that the Equalizer is connected and powered so it supplies the data-request signal and RX pull-up. If invalid frames or UART errors rise, check signal inversion, the pull-up/interface wiring and voltage, common ground, baud rate, and that the meter is using the ASCII profile. Failure to connect to Wi-Fi or MQTT does not stop reception.

Protocol references: [Swedish customer-interface recommendation v2.0](https://www.energiforetagen.se/globalassets/energiforetagen/det-erbjuder-vi/kurser-och-konferenser/elnat/branschrekommendation-lokalt-granssnitt-v2_0-201912.pdf), [Home Assistant MQTT discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery), and [Home Assistant sensor metadata](https://www.home-assistant.io/integrations/sensor/).
