// Exercise the firmware's actual address selection with controlled DNS and TCP
// failures. The ESP32 adapter supplies timeouts and socket abort/flush handling.
#[path = "../../../firmware/src/broker.rs"]
mod broker;

use broker::{Error, Family, Transport};
use han_config::MqttSettings;
use heapless::Vec;
use std::{collections::VecDeque, net::IpAddr};

enum Step {
    Resolve(Family, Result<Vec<IpAddr, 4>, Error>),
    Connect(IpAddr, Result<(), Error>),
}

struct Network {
    v4: bool,
    v6: bool,
    steps: VecDeque<Step>,
}

impl Network {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            v4: true,
            v6: true,
            steps: steps.into_iter().collect(),
        }
    }

    async fn run(&mut self, host: &str) -> Result<(), Error> {
        let result = broker::connect(self, &MqttSettings::new(host, 1884, "u", "p").unwrap()).await;
        assert!(self.steps.is_empty(), "expected more connection attempts");
        result
    }
}

impl Transport for Network {
    fn configured(&self, family: Family) -> bool {
        match family {
            Family::Ipv4 => self.v4,
            Family::Ipv6 => self.v6,
        }
    }

    async fn resolve(&mut self, host: &str, family: Family) -> Result<Vec<IpAddr, 4>, Error> {
        assert_eq!(host, "broker.example");
        let Some(Step::Resolve(expected, result)) = self.steps.pop_front() else {
            panic!("unexpected DNS lookup");
        };
        assert_eq!(family, expected);
        result
    }

    async fn connect(&mut self, address: IpAddr, port: u16) -> Result<(), Error> {
        assert_eq!(port, 1884);
        let Some(Step::Connect(expected, result)) = self.steps.pop_front() else {
            panic!("unexpected TCP attempt");
        };
        assert_eq!(address, expected);
        result
    }
}

fn ip(value: &str) -> IpAddr {
    value.parse().unwrap()
}

fn answer(family: Family, values: &[&str]) -> Step {
    Step::Resolve(family, Ok(values.iter().map(|s| ip(s)).collect()))
}

#[tokio::test]
async fn literals_bypass_dns_including_link_local_without_router_configuration() {
    for (host, address) in [
        ("192.0.2.10", "192.0.2.10"),
        ("2001:db8::10", "2001:db8::10"),
        ("[2001:db8::10]", "2001:db8::10"),
        ("fe80::10", "fe80::10"),
    ] {
        let mut network = Network::new([Step::Connect(ip(address), Ok(()))]);
        network.v4 = false;
        network.v6 = false;
        assert_eq!(network.run(host).await, Ok(()));
    }
}

#[tokio::test]
async fn ipv6_is_preferred_and_success_stops_fallback() {
    let mut network = Network::new([
        answer(Family::Ipv6, &["2001:db8::10"]),
        Step::Connect(ip("2001:db8::10"), Ok(())),
    ]);
    assert_eq!(network.run("broker.example").await, Ok(()));
}

#[tokio::test]
async fn absent_or_timed_out_aaaa_answers_fall_back_to_ipv4() {
    for result in [Ok(Vec::new()), Err(Error::Dns), Err(Error::Timeout)] {
        let mut network = Network::new([
            Step::Resolve(Family::Ipv6, result),
            answer(Family::Ipv4, &["192.0.2.10"]),
            Step::Connect(ip("192.0.2.10"), Ok(())),
        ]);
        assert_eq!(network.run("broker.example").await, Ok(()));
    }
}

#[tokio::test]
async fn tries_remaining_addresses_after_tcp_failure_or_timeout() {
    let mut network = Network::new([
        answer(Family::Ipv6, &["2001:db8::10", "2001:db8::11"]),
        Step::Connect(ip("2001:db8::10"), Err(Error::Timeout)),
        Step::Connect(ip("2001:db8::11"), Err(Error::Tcp)),
        answer(Family::Ipv4, &["192.0.2.10", "192.0.2.11"]),
        Step::Connect(ip("192.0.2.10"), Err(Error::Tcp)),
        Step::Connect(ip("192.0.2.11"), Ok(())),
    ]);
    assert_eq!(network.run("broker.example").await, Ok(()));
}

#[tokio::test]
async fn retries_other_ipv6_addresses_before_falling_back() {
    let mut network = Network::new([
        answer(Family::Ipv6, &["2001:db8::10", "2001:db8::11"]),
        Step::Connect(ip("2001:db8::10"), Ok(())),
    ]);
    // No failed IPv6 endpoint in this case: do not attempt later answers.
    assert_eq!(network.run("broker.example").await, Ok(()));
    let mut network = Network::new([
        answer(Family::Ipv6, &["2001:db8::10", "2001:db8::11"]),
        Step::Connect(ip("2001:db8::10"), Err(Error::Tcp)),
        Step::Connect(ip("2001:db8::11"), Ok(())),
    ]);
    assert_eq!(network.run("broker.example").await, Ok(()));
}

#[tokio::test]
async fn ipv4_only_network_does_not_wait_for_ipv6_dns_or_connections() {
    let mut network = Network::new([
        answer(Family::Ipv4, &["192.0.2.10"]),
        Step::Connect(ip("192.0.2.10"), Ok(())),
    ]);
    network.v6 = false;
    assert_eq!(network.run("broker.example").await, Ok(()));
}

#[tokio::test]
async fn ipv6_only_network_does_not_require_ipv4_configuration() {
    let mut network = Network::new([
        answer(Family::Ipv6, &["2001:db8::10"]),
        Step::Connect(ip("2001:db8::10"), Ok(())),
    ]);
    network.v4 = false;
    assert_eq!(network.run("broker.example").await, Ok(()));
}

#[tokio::test]
async fn connection_errors_are_not_hidden_by_missing_records_in_the_other_family() {
    let mut network = Network::new([
        answer(Family::Ipv6, &["2001:db8::10"]),
        Step::Connect(ip("2001:db8::10"), Err(Error::Timeout)),
        Step::Resolve(Family::Ipv4, Err(Error::Dns)),
    ]);
    assert_eq!(network.run("broker.example").await, Err(Error::Timeout));
    let mut network = Network::new([
        answer(Family::Ipv6, &[]),
        Step::Resolve(Family::Ipv4, Err(Error::Dns)),
    ]);
    assert_eq!(network.run("broker.example").await, Err(Error::Dns));
}
