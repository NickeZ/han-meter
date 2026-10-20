//! Packet-level regression for the stack used by the firmware. No Wi-Fi or
//! operating-system network configuration is needed.
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    phy::{ChecksumCapabilities, Device, Loopback, Medium, TxToken},
    time::{Duration, Instant},
    wire::{
        EthernetAddress, EthernetFrame, EthernetProtocol, Icmpv6Packet, Icmpv6Repr, IpAddress,
        IpCidr, IpProtocol, IpRepr, Ipv4Address, Ipv6Cidr, Ipv6Repr, NdiscPrefixInfoFlags,
        NdiscPrefixInformation, NdiscRepr, NdiscRouterFlags,
    },
};

#[test]
fn slaac_keeps_both_gateways_and_all_three_addresses_when_ipv4_is_already_up() {
    let mut device = Loopback::new(Medium::Ethernet);
    let mac = EthernetAddress([2, 0, 0, 0, 0, 2]);
    let mut config = Config::new(mac.into());
    config.slaac = true;
    let mut iface = Interface::new(config, &mut device, Instant::ZERO);
    let local_v4 = IpCidr::new(Ipv4Address::new(192, 0, 2, 2).into(), 24);
    let gateway_v4 = Ipv4Address::new(192, 0, 2, 1);
    let link_local =
        Ipv6Cidr::from_link_prefix(&Ipv6Cidr::new("fe80::".parse().unwrap(), 64), mac.into())
            .unwrap();
    iface.update_ip_addrs(|addresses| {
        addresses.push(local_v4).unwrap();
        addresses.push(link_local.into()).unwrap();
    });
    iface
        .routes_mut()
        .add_default_ipv4_route(gateway_v4)
        .unwrap();
    let gateway_v6 = "fe80::1".parse().unwrap();
    let advertisement = Icmpv6Repr::Ndisc(NdiscRepr::RouterAdvert {
        hop_limit: 64,
        flags: NdiscRouterFlags::empty(),
        router_lifetime: Duration::from_secs(600),
        reachable_time: Duration::ZERO,
        retrans_time: Duration::ZERO,
        lladdr: None,
        mtu: None,
        prefix_info: Some(NdiscPrefixInformation {
            prefix: "2001:db8:1::".parse().unwrap(),
            prefix_len: 64,
            flags: NdiscPrefixInfoFlags::ADDRCONF | NdiscPrefixInfoFlags::ON_LINK,
            valid_lifetime: Duration::from_secs(600),
            preferred_lifetime: Duration::from_secs(300),
        }),
    });
    let ip = IpRepr::Ipv6(Ipv6Repr {
        src_addr: gateway_v6,
        dst_addr: link_local.address(),
        next_header: IpProtocol::Icmpv6,
        hop_limit: 255,
        payload_len: advertisement.buffer_len(),
    });
    device.transmit(Instant::ZERO).unwrap().consume(
        14 + ip.header_len() + advertisement.buffer_len(),
        |bytes| {
            let mut frame = EthernetFrame::new_unchecked(bytes);
            frame.set_dst_addr(mac);
            frame.set_src_addr(EthernetAddress([2, 0, 0, 0, 0, 1]));
            frame.set_ethertype(EthernetProtocol::Ipv6);
            ip.emit(frame.payload_mut(), &ChecksumCapabilities::default());
            advertisement.emit(
                &gateway_v6,
                &link_local.address(),
                &mut Icmpv6Packet::new_unchecked(&mut frame.payload_mut()[ip.header_len()..]),
                &ChecksumCapabilities::default(),
            );
        },
    );
    let mut sockets = SocketSet::new(&mut [][..]);
    iface.poll(Instant::from_millis(1), &mut device, &mut sockets);
    // SLAAC synchronizes the received advertisement on the following poll.
    iface.poll(Instant::from_millis(2), &mut device, &mut sockets);

    assert!(iface.ip_addrs().contains(&local_v4));
    assert!(iface.ip_addrs().contains(&link_local.into()));
    let slaac = Ipv6Cidr::from_link_prefix(
        &Ipv6Cidr::new("2001:db8:1::".parse().unwrap(), 64),
        mac.into(),
    )
    .unwrap();
    assert!(iface.ip_addrs().contains(&slaac.into()));
    assert_eq!(
        iface.routes().get_default_ipv4_route().unwrap().via_router,
        IpAddress::Ipv4(gateway_v4)
    );
    assert_eq!(
        iface
            .routes()
            .get_default_ipv6_route()
            .map(|route| route.via_router),
        Some(IpAddress::Ipv6(gateway_v6)),
        "SLAAC must install the IPv6 gateway alongside the existing IPv4 route"
    );
}
