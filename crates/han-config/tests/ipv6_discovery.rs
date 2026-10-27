//! Exercise the firmware's actual SLAAC packet handling with lost router
//! solicitations and an otherwise idle network, without a Wi-Fi device.
use smoltcp::{
    iface::{Config, Interface, SocketSet, SocketStorage},
    phy::{ChecksumCapabilities, Device, Loopback, Medium, RxToken, TxToken},
    socket::tcp,
    time::{Duration, Instant},
    wire::{
        EthernetAddress, EthernetFrame, EthernetProtocol, Icmpv6Message, Icmpv6Packet, Icmpv6Repr,
        IpCidr, IpProtocol, IpRepr, Ipv4Address, Ipv6Address, Ipv6Cidr, Ipv6Packet, Ipv6Repr,
        NdiscPrefixInfoFlags, NdiscPrefixInformation, NdiscRepr, NdiscRouterFlags,
    },
};

const MAC: EthernetAddress = EthernetAddress([2, 0, 0, 0, 0, 2]);

fn setup() -> (Interface, Loopback) {
    let mut device = Loopback::new(Medium::Ethernet);
    let mut config = Config::new(MAC.into());
    config.slaac = true;
    config.random_seed = 42;
    let mut iface = Interface::new(config, &mut device, Instant::ZERO);
    let link_local =
        Ipv6Cidr::from_link_prefix(&Ipv6Cidr::new("fe80::".parse().unwrap(), 64), MAC.into())
            .unwrap();
    iface.update_ip_addrs(|addresses| {
        addresses
            .push(IpCidr::new(Ipv4Address::new(192, 0, 2, 2).into(), 24))
            .unwrap();
        addresses.push(link_local.into()).unwrap();
    });
    iface
        .routes_mut()
        .add_default_ipv4_route(Ipv4Address::new(192, 0, 2, 1))
        .unwrap();
    (iface, device)
}

fn drain_solicitations(device: &mut Loopback, now: Instant) -> usize {
    let mut count = 0;
    while let Some((rx, _)) = device.receive(now) {
        rx.consume(|bytes| {
            let ethernet = EthernetFrame::new_checked(bytes).unwrap();
            assert_eq!(ethernet.ethertype(), EthernetProtocol::Ipv6);
            let ip = Ipv6Packet::new_checked(ethernet.payload()).unwrap();
            assert_eq!(ip.dst_addr(), "ff02::2".parse::<Ipv6Address>().unwrap());
            assert_eq!(ip.hop_limit(), 255);
            let icmp = Icmpv6Packet::new_checked(ip.payload()).unwrap();
            assert_eq!(icmp.msg_type(), Icmpv6Message::RouterSolicit);
            count += 1;
        });
    }
    count
}

fn advertise(device: &mut Loopback, now: Instant, lifetime: Duration) {
    let source = "fe80::1".parse().unwrap();
    let destination = "ff02::1".parse().unwrap();
    let advertisement = Icmpv6Repr::Ndisc(NdiscRepr::RouterAdvert {
        hop_limit: 64,
        flags: NdiscRouterFlags::empty(),
        router_lifetime: lifetime,
        reachable_time: Duration::ZERO,
        retrans_time: Duration::ZERO,
        lladdr: None,
        mtu: None,
        prefix_info: Some(NdiscPrefixInformation {
            prefix: "2001:db8:1::".parse().unwrap(),
            prefix_len: 64,
            flags: NdiscPrefixInfoFlags::ADDRCONF | NdiscPrefixInfoFlags::ON_LINK,
            valid_lifetime: Duration::from_secs(7200),
            preferred_lifetime: Duration::from_secs(3600),
        }),
    });
    let ip = IpRepr::Ipv6(Ipv6Repr {
        src_addr: source,
        dst_addr: destination,
        next_header: IpProtocol::Icmpv6,
        hop_limit: 255,
        payload_len: advertisement.buffer_len(),
    });
    device.transmit(now).unwrap().consume(
        14 + ip.header_len() + advertisement.buffer_len(),
        |bytes| {
            let mut frame = EthernetFrame::new_unchecked(bytes);
            frame.set_dst_addr(EthernetAddress([0x33, 0x33, 0, 0, 0, 1]));
            frame.set_src_addr(EthernetAddress([2, 0, 0, 0, 0, 1]));
            frame.set_ethertype(EthernetProtocol::Ipv6);
            ip.emit(frame.payload_mut(), &ChecksumCapabilities::default());
            advertisement.emit(
                &source,
                &destination,
                &mut Icmpv6Packet::new_unchecked(&mut frame.payload_mut()[ip.header_len()..]),
                &ChecksumCapabilities::default(),
            );
        },
    );
}

#[test]
fn idle_network_schedules_router_discovery() {
    let (mut iface, mut device) = setup();
    let mut sockets = SocketSet::new(&mut [][..]);
    iface.poll(Instant::ZERO, &mut device, &mut sockets);
    assert_eq!(drain_solicitations(&mut device, Instant::ZERO), 1);
    let next = iface
        .poll_at(Instant::ZERO, &sockets)
        .expect("SLAAC retry needs a timer even with no socket deadlines");
    assert!(next > Instant::ZERO && next <= Instant::from_secs(5));
}

#[test]
fn losing_three_requests_does_not_require_a_reboot() {
    let (mut iface, mut device) = setup();
    let mut sockets = SocketSet::new(&mut [][..]);
    let mut requests = 0;
    let mut answered = false;
    for seconds in 0..120 {
        let now = Instant::from_secs(seconds);
        iface.poll(now, &mut device, &mut sockets);
        let sent = drain_solicitations(&mut device, now);
        requests += sent;
        if requests > 3 && !answered {
            advertise(&mut device, now, Duration::from_secs(600));
            answered = true;
        } else if answered {
            assert_eq!(sent, 0, "stop soliciting after the router responds");
        }
    }
    assert!(
        answered,
        "discovery gave up after the first three lost requests"
    );
    assert_eq!(requests, 4);
    assert!(iface.routes().get_default_ipv6_route().is_some());
    assert!(iface.routes().get_default_ipv4_route().is_some());
    assert_eq!(iface.ip_addrs().len(), 3);
}

#[test]
fn retries_back_off_and_keep_future_deadlines_on_an_ipv4_only_network() {
    let (mut iface, mut device) = setup();
    let mut sockets = SocketSet::new(&mut [][..]);
    let mut now = Instant::ZERO;
    for attempt in 0..24 {
        iface.poll(now, &mut device, &mut sockets);
        assert_eq!(drain_solicitations(&mut device, now), 1);
        let next = iface
            .poll_at(now, &sockets)
            .expect("discovery must keep retrying");
        let delay = next - now;
        assert!(delay >= Duration::from_millis(3600), "no busy polling");
        assert!(
            delay <= Duration::from_secs(3960),
            "RFC 7559 maximum plus jitter"
        );
        if attempt >= 12 {
            assert!(
                delay >= Duration::from_secs(3240),
                "long outages back off to about an hour"
            );
        }
        iface.poll(next - Duration::from_millis(1), &mut device, &mut sockets);
        assert_eq!(
            drain_solicitations(&mut device, next),
            0,
            "don't transmit before the deadline"
        );
        now = next;
    }
}

#[test]
fn zero_router_lifetime_does_not_stop_discovery() {
    let (mut iface, mut device) = setup();
    let mut sockets = SocketSet::new(&mut [][..]);
    iface.poll(Instant::ZERO, &mut device, &mut sockets);
    drain_solicitations(&mut device, Instant::ZERO);
    advertise(&mut device, Instant::ZERO, Duration::ZERO);
    iface.poll(Instant::from_millis(1), &mut device, &mut sockets);
    iface.poll(Instant::from_millis(2), &mut device, &mut sockets);
    let next = iface.poll_at(Instant::from_millis(2), &sockets).unwrap();
    assert!(next <= Instant::from_secs(5));
    iface.poll(next, &mut device, &mut sockets);
    assert_eq!(drain_solicitations(&mut device, next), 1);
    assert!(iface.routes().get_default_ipv6_route().is_none());
}

#[test]
fn socket_deadlines_survive_when_slaac_has_no_timer() {
    let (mut iface, mut device) = setup();
    let mut storage = [SocketStorage::EMPTY];
    let mut sockets = SocketSet::new(&mut storage[..]);
    iface.poll(Instant::ZERO, &mut device, &mut sockets);
    drain_solicitations(&mut device, Instant::ZERO);
    advertise(&mut device, Instant::ZERO, Duration::from_secs(600));
    iface.poll(Instant::from_millis(1), &mut device, &mut sockets);
    iface.poll(Instant::from_millis(2), &mut device, &mut sockets);
    // Expire all learned IPv6 information. The IPv4 TCP connection must still
    // get its retry timer when the maintaining SLAAC state has no deadline.
    let now = Instant::from_secs(7201);
    iface.poll(now, &mut device, &mut sockets);
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; 64]),
        tcp::SocketBuffer::new(vec![0; 64]),
    );
    socket
        .connect(
            iface.context(),
            (Ipv4Address::new(192, 0, 2, 10), 1883),
            50000,
        )
        .unwrap();
    sockets.add(socket);
    iface.poll(now, &mut device, &mut sockets);
    let next = iface
        .poll_at(now, &sockets)
        .expect("SLAAC must not mask a TCP retry deadline");
    assert!(next <= now + Duration::from_secs(10));
}
