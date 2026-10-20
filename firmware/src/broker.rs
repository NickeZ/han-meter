//! Broker address selection, shared with host tests.
use core::net::IpAddr;
use han_config::MqttSettings;
use heapless::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Ipv6,
    Ipv4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Dns,
    Tcp,
    Timeout,
}

pub(crate) trait Transport {
    fn configured(&self, family: Family) -> bool;
    async fn resolve(&mut self, host: &str, family: Family) -> Result<Vec<IpAddr, 4>, Error>;
    /// Bound the attempt and reset the socket on failure before returning.
    async fn connect(&mut self, address: IpAddr, port: u16) -> Result<(), Error>;
}

pub(crate) async fn connect(
    transport: &mut impl Transport,
    settings: &MqttSettings,
) -> Result<(), Error> {
    if let Some(address) = settings.ip_address() {
        return transport.connect(address, settings.port).await;
    }

    let mut dns_error = Error::Dns;
    let mut connection_error = None;
    // Prefer IPv6 when configured, but a missing AAAA record, broken route, or
    // unreachable endpoint must not prevent trying the remaining addresses.
    for family in [Family::Ipv6, Family::Ipv4] {
        if !transport.configured(family) {
            continue;
        }
        let addresses = match transport.resolve(&settings.host, family).await {
            Ok(addresses) => addresses,
            Err(error) => {
                dns_error = error;
                continue;
            }
        };
        for address in addresses {
            match transport.connect(address, settings.port).await {
                Ok(()) => return Ok(()),
                Err(error) => connection_error = Some(error),
            }
        }
    }
    // If DNS returned a usable address, report its connection error even when
    // the other address family has no DNS records.
    Err(connection_error.unwrap_or(dns_error))
}
