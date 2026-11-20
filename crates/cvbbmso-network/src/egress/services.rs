//! Reads narrowly scoped operating system service destinations.

use std::net::IpAddr;

use super::policy::{Service, Transport};
use super::resolver::Resolver;
use crate::NetworkError;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(windows)]
use self::windows as platform;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Services {
    rules: Vec<Service>,
    time_hosts: Vec<String>,
}

impl Services {
    pub(crate) fn discover() -> Result<Self, NetworkError> {
        #[cfg(any(target_os = "macos", windows))]
        {
            let mut services = platform::discover()?;
            services.rules.sort_unstable();
            services.rules.dedup();
            services.time_hosts.sort_unstable();
            services.time_hosts.dedup();
            Ok(services)
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        Err(NetworkError::Platform)
    }

    pub(crate) fn rules(&self) -> &[Service] {
        &self.rules
    }

    pub(crate) async fn resolve(&self, resolver: &impl Resolver) -> Vec<Service> {
        let mut destinations = self.rules.clone();
        for host in &self.time_hosts {
            if let Ok(addresses) = resolver.resolve(host, 123).await {
                destinations.extend(
                    addresses
                        .into_iter()
                        .filter(|ip| is_unicast(*ip))
                        .map(|address| Service::new(address, 123, Transport::Udp)),
                );
            }
        }
        destinations.sort_unstable();
        destinations.dedup();
        destinations
    }

    #[cfg(any(target_os = "macos", windows))]
    fn add(&mut self, address: IpAddr, port: u16, transport: Transport) {
        if is_unicast(address) && port != 0 {
            self.rules.push(Service::new(address, port, transport));
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    fn add_plain_dns(&mut self, address: IpAddr, port: u16) {
        for transport in [Transport::Tcp, Transport::Udp] {
            self.add(address, port, transport);
        }
    }
}

pub(super) fn is_unicast(ip: IpAddr) -> bool {
    !ip.is_unspecified()
        && !ip.is_multicast()
        && !matches!(ip, IpAddr::V4(ip) if ip.is_broadcast())
        && !matches!(ip, IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some())
}

#[cfg(any(target_os = "macos", windows))]
fn parse_address(value: &str) -> Option<IpAddr> {
    value.split('%').next()?.parse().ok()
}
