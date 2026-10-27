// Exercise the firmware's actual HTTP adapter, including cancelled reads and
// reuse, with independent socket-owner, handler, and browser futures.
#[path = "../../../firmware/src/http_socket.rs"]
mod http_socket;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embedded_io_adapters::tokio_1::FromTokio;
use han_logs::io_bridge::{Bridge, CHUNK_BYTES};
use http_socket::{Close, Connection, Control};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BODY: &str = include_str!("../../han-config/src/settings.html");

#[tokio::test]
async fn http_timeout_then_fragmented_requests_reuse_the_same_bridges() {
    let (mut rx, mut rx_owner) = Box::leak(Box::new(
        Bridge::<CriticalSectionRawMutex, CHUNK_BYTES>::new(),
    ))
    .split();
    let (mut tx, mut tx_owner) = Box::leak(Box::new(
        Bridge::<CriticalSectionRawMutex, CHUNK_BYTES>::new(),
    ))
    .split();
    let control = Control::new();
    // Stalled headers and stalled readers must abort. Neither cancelled read
    // nor write requests/replies may leak into subsequent connections.
    for (stall_headers, stall_response) in
        [(true, false), (false, true), (false, false), (false, false)]
    {
        let stalled = stall_headers || stall_response;
        let (socket, mut browser) = tokio::io::duplex(53);
        let router =
            picoserve::Router::new().route("/", picoserve::routing::get(|| async { BODY }));
        let config = picoserve::Config::new(picoserve::Timeouts {
            start_read_request: picoserve::time::Duration::from_millis(50),
            persistent_start_read_request: picoserve::time::Duration::from_millis(50),
            read_request: picoserve::time::Duration::from_millis(50),
            write: picoserve::time::Duration::from_millis(100),
        })
        .close_connection_after_response();
        let mut buffer = [0; 6144];
        let handler =
            picoserve::Server::new_tokio(&router, &config, &mut buffer).serve(Connection {
                rx: &mut rx,
                tx: &mut tx,
                control: &control,
            });
        let owner = async {
            let (reader, writer) = tokio::io::split(socket);
            let mut reader = FromTokio::new(reader);
            let mut writer = FromTokio::new(writer);
            let mode = tokio::select! {
                mode = control.close.receive() => mode,
                _ = rx_owner.serve_read(&mut reader) => unreachable!(),
                _ = tx_owner.serve_write(&mut writer) => unreachable!(),
            };
            assert_eq!(
                mode,
                if stalled {
                    Close::Abort
                } else {
                    Close::Graceful
                }
            );
            drop(reader);
            drop(writer);
            rx_owner.reset();
            tx_owner.reset();
            control.closed.send(()).await;
        };
        let browser = async {
            if stall_headers {
                browser.write_all(b"GET / HTTP/1.1\r\nHost:").await.unwrap();
            } else {
                for chunk in b"GET / HTTP/1.1\r\nHost: device\r\n\r\n".chunks(3) {
                    browser.write_all(chunk).await.unwrap();
                    tokio::task::yield_now().await;
                }
            }
            if stall_response {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let mut response = Vec::new();
            browser.read_to_end(&mut response).await.unwrap();
            if !stalled {
                let response = String::from_utf8(response).unwrap();
                assert!(response.starts_with("HTTP/1.1 200"), "{response}");
                assert!(response.ends_with(BODY));
            }
        };
        let (result, (), ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(handler, owner, browser)
        })
        .await
        .unwrap();
        assert_eq!(result.is_err(), stalled);
    }
}

#[tokio::test]
async fn pending_http_read_does_not_block_writing_a_response() {
    use embedded_io_async::{Read, Write};
    let mut rx_bridge = Bridge::<CriticalSectionRawMutex, 4>::new();
    let mut tx_bridge = Bridge::<CriticalSectionRawMutex, 4>::new();
    let (mut rx, mut rx_owner) = rx_bridge.split();
    let (mut tx, mut tx_owner) = tx_bridge.split();
    let (socket, mut peer) = tokio::io::duplex(7);
    let (reader, writer) = tokio::io::split(socket);
    let mut reader = FromTokio::new(reader);
    let mut writer = FromTokio::new(writer);
    let exchange = async {
        let pending_read = async {
            let mut bytes = [0; 4];
            rx.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"done");
        };
        let response = async {
            tx.write_all(b"response before read completes")
                .await
                .unwrap();
            tx.flush().await.unwrap();
        };
        let peer = async {
            let mut response = [0; 30];
            peer.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"response before read completes");
            peer.write_all(b"done").await.unwrap();
        };
        tokio::join!(pending_read, response, peer);
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            () = exchange => {},
            _ = rx_owner.serve_read(&mut reader) => unreachable!(),
            _ = tx_owner.serve_write(&mut writer) => unreachable!(),
        }
    })
    .await
    .unwrap();
}
