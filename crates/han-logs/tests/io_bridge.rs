use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embedded_io_adapters::tokio_1::FromTokio;
use embedded_io_async::{ErrorKind, ErrorType, Read, Write};
use han_logs::io_bridge::Bridge;
use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[tokio::test]
async fn fragmented_round_trip_flush_empty_buffers_and_eof() {
    let mut bridge = Bridge::<CriticalSectionRawMutex, 37>::new();
    let (mut client, mut server) = bridge.split();
    let (socket, mut peer) = tokio::io::duplex(53);
    let mut socket = FromTokio::new(socket);
    let payload: Vec<_> = (0..4096).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();
    let exchange = async {
        let user = async {
            assert_eq!(client.write(&[]).await.unwrap(), 0);
            assert_eq!(client.read(&mut []).await.unwrap(), 0);
            client.write_all(&payload).await.unwrap();
            client.flush().await.unwrap();
            let mut result = vec![0; payload.len()];
            client.read_exact(&mut result).await.unwrap();
            assert_eq!(result, payload);
            assert_eq!(client.read(&mut [0; 3]).await.unwrap(), 0);
        };
        let peer = async {
            let mut request = vec![0; expected.len()];
            peer.read_exact(&mut request).await.unwrap();
            assert_eq!(request, expected);
            for chunk in request.chunks(11) {
                peer.write_all(chunk).await.unwrap();
            }
            peer.shutdown().await.unwrap();
        };
        tokio::join!(user, peer);
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            () = exchange => {},
            _ = server.serve(&mut socket) => unreachable!(),
        }
    })
    .await
    .unwrap();
}

struct Broken;
impl ErrorType for Broken {
    type Error = ErrorKind;
}
impl Read for Broken {
    async fn read(&mut self, _: &mut [u8]) -> Result<usize, ErrorKind> {
        Err(ErrorKind::ConnectionReset)
    }
}
impl Write for Broken {
    async fn write(&mut self, _: &[u8]) -> Result<usize, ErrorKind> {
        Err(ErrorKind::BrokenPipe)
    }
    async fn flush(&mut self) -> Result<(), ErrorKind> {
        Err(ErrorKind::TimedOut)
    }
}

#[tokio::test]
async fn socket_errors_reach_the_tls_client() {
    let mut bridge = Bridge::<CriticalSectionRawMutex, 32>::new();
    let (mut client, mut server) = bridge.split();
    let mut socket = Broken;
    tokio::select! {
        () = async {
            assert_eq!(client.read(&mut [0; 1]).await, Err(ErrorKind::ConnectionReset));
            assert_eq!(client.write(b"x").await, Err(ErrorKind::BrokenPipe));
            assert_eq!(client.flush().await, Err(ErrorKind::TimedOut));
        } => {},
        _ = server.serve(&mut socket) => unreachable!(),
    }
}

#[tokio::test]
async fn late_reply_from_a_cancelled_call_cannot_satisfy_the_next_call() {
    let mut bridge = Bridge::<CriticalSectionRawMutex, 32>::new();
    let (mut client, mut server) = bridge.split();
    let mut socket = Broken;
    let mut serving = pin!(server.serve(&mut socket));
    {
        let mut bytes = [0; 1];
        let mut read = pin!(client.read(&mut bytes));
        assert!(poll_once(read.as_mut()).is_pending());
        assert!(poll_once(serving.as_mut()).is_pending());
        // The read error is queued, but the client is cancelled before taking it.
    }
    tokio::select! {
        result = client.flush() => assert_eq!(result, Err(ErrorKind::TimedOut)),
        _ = serving => unreachable!(),
    }
}

#[tokio::test]
async fn closing_a_stalled_connection_discards_requests_and_replies_before_reuse() {
    let mut bridge = Bridge::<CriticalSectionRawMutex, 32>::new();
    let (mut client, mut server) = bridge.split();
    let (socket, _silent_peer) = tokio::io::duplex(32);
    let mut socket = FromTokio::new(socket);
    {
        let mut bytes = [0; 4];
        let mut read = pin!(client.read(&mut bytes));
        let mut serving = pin!(server.serve(&mut socket));
        assert!(poll_once(read.as_mut()).is_pending());
        assert!(poll_once(serving.as_mut()).is_pending());
        // Both futures are dropped, as in the firmware's TLS timeout + Close.
    }
    server.reset();
    // Also discard a request that never reached the old socket owner.
    {
        let mut write = pin!(client.write(b"stale"));
        assert!(poll_once(write.as_mut()).is_pending());
    }
    server.reset();
    let (socket, mut peer) = tokio::io::duplex(32);
    let mut socket = FromTokio::new(socket);
    peer.write_all(b"fresh").await.unwrap();
    tokio::select! {
        () = async {
            let mut bytes = [0; 5];
            client.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"fresh");
            assert!(tokio::time::timeout(Duration::from_millis(10), peer.read_u8()).await.is_err());
        } => {},
        _ = server.serve(&mut socket) => unreachable!(),
    }
}

#[test]
fn endpoints_work_across_threads_without_moving_the_socket() {
    let bridge = Box::leak(Box::new(Bridge::<CriticalSectionRawMutex, 17>::new()));
    let (mut client, mut server) = bridge.split();
    let worker = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                client.write_all(b"background TLS bytes").await.unwrap();
                let mut reply = [0; 3];
                client.read_exact(&mut reply).await.unwrap();
                assert_eq!(&reply, b"ack");
            });
    });
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (socket, mut peer) = tokio::io::duplex(32);
            let mut socket = FromTokio::new(socket);
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::select! {
                    () = async {
                        let mut bytes = [0; 20];
                        peer.read_exact(&mut bytes).await.unwrap();
                        assert_eq!(&bytes, b"background TLS bytes");
                        peer.write_all(b"ack").await.unwrap();
                        while !worker.is_finished() { tokio::task::yield_now().await; }
                    } => {},
                    _ = server.serve(&mut socket) => unreachable!(),
                }
            })
            .await
            .unwrap();
        });
    worker.join().unwrap();
}
