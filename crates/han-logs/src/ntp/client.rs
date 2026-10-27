//! Bounded address selection for SNTP, independent of the network adapter.
use core::{future::Future, net::IpAddr};
use heapless::Vec;

use super::ResponseError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Ipv6,
    Ipv4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NoAddress,
    Dns(Family),
    DnsTimeout(Family),
    Bind,
    Send(IpAddr),
    Receive(IpAddr),
    Timeout(IpAddr),
    Rejected(IpAddr, ResponseError),
}

pub trait Transport {
    fn configured(&self, family: Family) -> bool;
    /// Bound each DNS lookup to five seconds, including unavailable query slots.
    fn resolve(
        &mut self,
        host: &str,
        family: Family,
    ) -> impl Future<Output = Result<Vec<IpAddr, 4>, Error>>;
    /// Bound each exchange to three seconds and discard its socket on failure.
    fn query(&mut self, address: IpAddr) -> impl Future<Output = Result<u64, Error>>;
}

pub async fn synchronize(transport: &mut impl Transport, host: &str) -> Result<u64, Error> {
    let literal = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(address) = literal.parse() {
        return transport.query(address).await;
    }

    let mut error = Error::NoAddress;
    let mut addresses = [Vec::<IpAddr, 4>::new(), Vec::new()];
    for (family, addresses) in [Family::Ipv6, Family::Ipv4].into_iter().zip(&mut addresses) {
        if transport.configured(family) {
            match transport.resolve(host, family).await {
                Ok(found) => *addresses = found,
                Err(reason) => error = reason,
            }
        }
    }
    // Alternate families so an unreachable IPv6 path doesn't delay every IPv4
    // candidate. The per-operation timeouts bound this to 34 seconds at most;
    // a shorter outer deadline would silently starve the remaining addresses.
    for index in 0..4 {
        for family in &addresses {
            if let Some(&address) = family.get(index) {
                match transport.query(address).await {
                    Ok(time) => return Ok(time),
                    Err(reason) => error = reason,
                }
            }
        }
    }
    Err(error)
}
