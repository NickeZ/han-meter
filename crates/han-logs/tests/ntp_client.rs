use han_logs::ntp::client::{self, Error, Family, Transport};
use heapless::Vec;
use std::{collections::VecDeque, net::IpAddr, time::Duration};
use tokio::time::{Instant, sleep};

const TIME: u64 = 1_800_000_000_000;

struct Network {
    v6: Result<Vec<IpAddr, 4>, Error>,
    v4: Result<Vec<IpAddr, 4>, Error>,
    attempts: VecDeque<(IpAddr, Result<u64, Error>)>,
    configured: bool,
}

impl Transport for Network {
    fn configured(&self, _: Family) -> bool {
        self.configured
    }

    async fn resolve(&mut self, host: &str, family: Family) -> Result<Vec<IpAddr, 4>, Error> {
        assert_eq!(host, "time.test");
        sleep(Duration::from_secs(5)).await;
        match family {
            Family::Ipv6 => self.v6.clone(),
            Family::Ipv4 => self.v4.clone(),
        }
    }

    async fn query(&mut self, address: IpAddr) -> Result<u64, Error> {
        let (expected, result) = self.attempts.pop_front().expect("unexpected NTP request");
        assert_eq!(address, expected);
        sleep(Duration::from_secs(3)).await;
        result
    }
}

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

#[tokio::test(start_paused = true)]
async fn unreachable_ipv6_does_not_starve_ipv4_or_late_dns_candidates() {
    let v6: Vec<IpAddr, 4> = (1..=4).map(|i| ip(&format!("2001:db8::{i}"))).collect();
    let v4: Vec<IpAddr, 4> = (1..=4).map(|i| ip(&format!("192.0.2.{i}"))).collect();
    let attempts = (0..4)
        .flat_map(|i| [v6[i], v4[i]])
        .map(|address| {
            (
                address,
                if address == v4[3] {
                    Ok(TIME)
                } else {
                    Err(Error::Timeout(address))
                },
            )
        })
        .collect();
    let mut network = Network {
        v6: Ok(v6),
        v4: Ok(v4),
        attempts,
        configured: true,
    };
    let start = Instant::now();
    assert_eq!(
        client::synchronize(&mut network, "time.test").await,
        Ok(TIME)
    );
    assert!(network.attempts.is_empty());
    // This valid fallback used to be cancelled by the 20-second outer timeout.
    assert_eq!(start.elapsed(), Duration::from_secs(34));
}

#[tokio::test(start_paused = true)]
async fn missing_aaaa_records_and_dns_timeouts_allow_ipv4() {
    for v6 in [
        Ok(Vec::new()),
        Err(Error::Dns(Family::Ipv6)),
        Err(Error::DnsTimeout(Family::Ipv6)),
    ] {
        let address = ip("192.0.2.1");
        let mut network = Network {
            v6,
            v4: Ok([address].into_iter().collect()),
            attempts: [(address, Ok(TIME))].into(),
            configured: true,
        };
        assert_eq!(
            client::synchronize(&mut network, "time.test").await,
            Ok(TIME)
        );
        assert!(network.attempts.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn literals_bypass_dns_including_bracketed_and_link_local_ipv6() {
    for (host, address) in [
        ("192.0.2.1", "192.0.2.1"),
        ("[2001:db8::1]", "2001:db8::1"),
        ("fe80::1", "fe80::1"),
    ] {
        let mut network = Network {
            v6: Err(Error::Dns(Family::Ipv6)),
            v4: Err(Error::Dns(Family::Ipv4)),
            attempts: [(ip(address), Ok(TIME))].into(),
            configured: false,
        };
        assert_eq!(client::synchronize(&mut network, host).await, Ok(TIME));
        assert!(network.attempts.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn failures_distinguish_dns_from_unresponsive_or_rejecting_servers() {
    let mut network = Network {
        v6: Err(Error::Dns(Family::Ipv6)),
        v4: Err(Error::DnsTimeout(Family::Ipv4)),
        attempts: VecDeque::new(),
        configured: true,
    };
    assert_eq!(
        client::synchronize(&mut network, "time.test").await,
        Err(Error::DnsTimeout(Family::Ipv4))
    );
    let address = ip("192.0.2.1");
    network.v4 = Ok([address].into_iter().collect());
    for error in [
        Error::Timeout(address),
        Error::Rejected(address, han_logs::ntp::ResponseError::Unsynchronized),
    ] {
        network.attempts.push_back((address, Err(error)));
        assert_eq!(
            client::synchronize(&mut network, "time.test").await,
            Err(error)
        );
        assert!(network.attempts.is_empty());
    }
}
