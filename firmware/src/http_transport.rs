//! TCP stays on the network thread; HTTP parsing/handlers stay in background.
use crate::http_socket::{Close, Connection, Control, Stream};
use alloc::vec::Vec;
use core::net::IpAddr;
use embassy_futures::select::{Either, select};
use embassy_net::{Stack, tcp::TcpSocket};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer, with_timeout};
use han_logs::io_bridge::{Bridge, CHUNK_BYTES, Server};
use static_cell::StaticCell;

pub(crate) type Addresses = [Option<IpAddr>; 3];
type SocketOwner = Server<'static, CriticalSectionRawMutex, CHUNK_BYTES>;
static CONTROL: Control = Control::new();
static ACCEPTED: Signal<CriticalSectionRawMutex, Addresses> = Signal::new();
pub(crate) static ADDRESSES: Signal<CriticalSectionRawMutex, Addresses> = Signal::new();

pub(crate) struct Client {
    rx: Stream,
    tx: Stream,
    buffer: Vec<u8>,
}

pub(crate) struct Owner {
    rx: SocketOwner,
    tx: SocketOwner,
    rx_buffer: Vec<u8>,
    tx_buffer: Vec<u8>,
}

pub(crate) fn init() -> Option<(Client, Owner)> {
    // Optional HTTP buffers must not consume static stack headroom or make
    // reception/MQTT fail when memory is tight.
    let buffer = crate::log_shipping::buffer(han_config::web::HTTP_BUFFER_SIZE).ok()?;
    let rx_buffer = crate::log_shipping::buffer(2048).ok()?;
    let tx_buffer = crate::log_shipping::buffer(2048).ok()?;
    static RX: StaticCell<Bridge<CriticalSectionRawMutex, CHUNK_BYTES>> = StaticCell::new();
    static TX: StaticCell<Bridge<CriticalSectionRawMutex, CHUNK_BYTES>> = StaticCell::new();
    let (rx, rx_owner) = RX.init_with(Bridge::new).split();
    let (tx, tx_owner) = TX.init_with(Bridge::new).split();
    Some((
        Client { rx, tx, buffer },
        Owner {
            rx: rx_owner,
            tx: tx_owner,
            rx_buffer,
            tx_buffer,
        },
    ))
}

impl Client {
    pub(crate) async fn accept(&mut self) -> (Connection<'_>, &mut [u8], Addresses) {
        let addresses = ACCEPTED.wait().await;
        (
            Connection {
                rx: &mut self.rx,
                tx: &mut self.tx,
                control: &CONTROL,
            },
            &mut self.buffer,
            addresses,
        )
    }
}

fn addresses(stack: Stack<'_>) -> Addresses {
    let v4 = stack.config_v4().map(|c| c.address.address().into());
    let v6 = stack.config_v6().map(|c| c.address.address().into());
    let link_local = embassy_net::Ipv6Cidr::from_link_prefix(
        &embassy_net::Ipv6Cidr::new(embassy_net::Ipv6Cidr::LINK_LOCAL_PREFIX.address(), 64),
        stack.hardware_address(),
    )
    .map(|c| c.address().into());
    [v4, v6, link_local]
}

#[embassy_executor::task]
pub(crate) async fn watch_addresses(stack: Stack<'static>) {
    let mut previous = [None; 3];
    loop {
        let current = addresses(stack);
        if current != previous {
            ADDRESSES.signal(current);
            previous = current;
        }
        Timer::after_secs(1).await;
    }
}

#[embassy_executor::task]
pub(crate) async fn run(stack: Stack<'static>, mut io: Owner) {
    loop {
        let mut socket = TcpSocket::new(stack, &mut io.rx_buffer, &mut io.tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(15)));
        if socket.accept(80).await.is_err() {
            Timer::after_millis(100).await;
            continue;
        }
        ACCEPTED.signal(addresses(stack));
        let mode = {
            let (mut reader, mut writer) = socket.split();
            let Either::First(mode) = select(
                CONTROL.close.receive(),
                select(
                    io.rx.serve_read(&mut reader),
                    io.tx.serve_write(&mut writer),
                ),
            )
            .await;
            mode
        };
        if mode == Close::Graceful {
            socket.close();
            let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
        }
        socket.abort();
        let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
        drop(socket);
        io.rx.reset();
        io.tx.reset();
        CONTROL.closed.send(()).await;
    }
}
