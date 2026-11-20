//! Computes the complement of the exam allowlist as terminal PF drop rules.
//! On ingress, reject TCP/UDP peers whose return packets could not be sent.
//! An allowed packet matches no filter here, so it reaches the host rules
//! with its original decision and metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::net::IpAddr;

use crate::egress::policy::{Plan, Service, Transport};

const LOOPBACK: &str = "cvbbmso_loopback";
const DESTINATIONS: &str = "cvbbmso_destinations";
const LOOPBACK_ADDRESSES: [&str; 2] = ["127.0.0.1", "::1"];

#[derive(PartialEq, Eq)]
pub(super) struct Rules {
    pub(super) source: String,
    pub(super) tables: BTreeMap<String, BTreeSet<String>>,
    services: BTreeSet<Service>,
}

impl Rules {
    pub(super) fn new(plan: &Plan) -> Self {
        let mut services = plan.services.iter().copied().collect::<BTreeSet<_>>();
        for target in [&plan.accoding, &plan.server] {
            for &ip in &target.addresses {
                services.insert(Service::new(ip, target.port, Transport::Tcp));
                services.insert(Service::new(ip, target.port, Transport::Udp));
            }
        }
        for remote in &plan.certificate {
            services.insert(Service {
                remote: *remote,
                transport: Transport::Tcp,
            });
        }
        services.retain(|service| !service.remote.ip().is_loopback());
        let mut rules = Self {
            source: String::new(),
            tables: BTreeMap::new(),
            services,
        };
        rules.render();
        rules
    }

    pub(super) fn revoked_destinations(&self, next: &Self) -> BTreeSet<IpAddr> {
        self.services
            .difference(&next.services)
            .map(|service| service.remote.ip())
            .collect()
    }

    fn render(&mut self) {
        let mut filters = String::new();
        let loopback = loopback_addresses();
        self.tables.insert(LOOPBACK.to_owned(), loopback.clone());

        let ndp = [
            (
                133,
                BTreeSet::from(["::1".to_owned(), "ff02::2".to_owned()]),
            ),
            (
                135,
                BTreeSet::from([
                    "::1".to_owned(),
                    "fe80::/10".to_owned(),
                    "ff02::1:ff00:0/104".to_owned(),
                ]),
            ),
            (
                136,
                BTreeSet::from([
                    "::1".to_owned(),
                    "fe80::/10".to_owned(),
                    "ff02::1".to_owned(),
                ]),
            ),
        ];
        let mut destinations = loopback;
        destinations.extend(
            self.services
                .iter()
                .map(|service| service.remote.ip().to_string()),
        );
        destinations.extend(
            ndp.iter()
                .flat_map(|(_, addresses)| addresses.iter().cloned()),
        );
        self.tables.insert(DESTINATIONS.to_owned(), destinations);
        let _ = writeln!(
            filters,
            "block drop out quick from any to ! <{DESTINATIONS}>"
        );
        let _ = writeln!(
            filters,
            "block drop out quick on ! lo0 from any to <{LOOPBACK}>"
        );
        let _ = writeln!(
            filters,
            "block drop out quick from any to ! <{LOOPBACK}> fragment"
        );
        let _ = writeln!(
            filters,
            "block drop in quick on ! lo0 proto {{ tcp udp }} from <{LOOPBACK}> to any"
        );
        let _ = writeln!(
            filters,
            "block drop in quick proto {{ tcp udp }} from ! <{LOOPBACK}> to any fragment"
        );

        // PF uses protocol zero as its wildcard. For IPv6 it processes the
        // Hop-by-Hop header before classifying the transport.
        let protocols = numbers((1..=255).filter(|number| !matches!(number, 6 | 17 | 58)));
        let _ = writeln!(
            filters,
            "block drop out quick proto {{ {protocols} }} from any to ! <{LOOPBACK}>"
        );

        self.render_transport(&mut filters, Transport::Tcp, "tcp");
        self.render_transport(&mut filters, Transport::Udp, "udp");

        // Apple's PF stores ICMP type plus one in a byte. Type 255 is
        // reserved and would encode a wildcard instead of an exact match.
        let ndp_destinations = ndp
            .iter()
            .flat_map(|(_, addresses)| addresses.iter().cloned())
            .collect();
        self.tables
            .insert("cvbbmso_icmp6".to_owned(), ndp_destinations);
        let _ = writeln!(
            filters,
            "block drop out quick inet6 proto ipv6-icmp from any to ! <cvbbmso_icmp6>"
        );
        let other_types = numbers((0..=254).filter(|kind| !matches!(kind, 133 | 135 | 136)));
        let _ = writeln!(
            filters,
            "block drop out quick inet6 proto ipv6-icmp from any to ! <{LOOPBACK}> icmp6-type {{ {other_types} }}"
        );
        for (kind, addresses) in ndp {
            let name = format!("cvbbmso_icmp6_{kind}");
            let _ = writeln!(
                filters,
                "block drop out quick inet6 proto ipv6-icmp from any to ! <{name}> icmp6-type {kind}"
            );
            self.tables.insert(name, addresses);
        }

        for (name, addresses) in &self.tables {
            let addresses = addresses
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            let _ = writeln!(self.source, "table <{name}> const {{ {addresses} }}");
        }
        self.source.push_str(&filters);
    }

    fn render_transport(&mut self, filters: &mut String, transport: Transport, protocol: &str) {
        let mut ports = BTreeMap::<u16, BTreeSet<String>>::new();
        for service in self
            .services
            .iter()
            .filter(|service| service.transport == transport)
        {
            ports
                .entry(service.remote.port())
                .or_insert_with(loopback_addresses)
                .insert(service.remote.ip().to_string());
        }

        if ports.is_empty() {
            let _ = writeln!(
                filters,
                "block drop out quick proto {protocol} from any to ! <{LOOPBACK}>"
            );
            let _ = writeln!(
                filters,
                "block drop in quick proto {protocol} from ! <{LOOPBACK}> to any"
            );
            return;
        }
        let mut start = 0;
        let mut gaps = Vec::new();
        for (port, addresses) in ports {
            let name = format!("cvbbmso_{protocol}_{port}");
            self.tables.insert(name.clone(), addresses);
            let _ = writeln!(
                filters,
                "block drop out quick proto {protocol} from any to ! <{name}> port {port}"
            );
            let _ = writeln!(
                filters,
                "block drop in quick proto {protocol} from ! <{name}> port {port} to any"
            );
            let port = u32::from(port);
            if start < port {
                gaps.push(port_range(start, port - 1));
            }
            start = port + 1;
        }
        if u16::try_from(start).is_ok() {
            gaps.push(port_range(start, u32::from(u16::MAX)));
        }
        if !gaps.is_empty() {
            let gaps = gaps.join(" ");
            let _ = writeln!(
                filters,
                "block drop out quick proto {protocol} from any to ! <{LOOPBACK}> port {{ {gaps} }}"
            );
            let _ = writeln!(
                filters,
                "block drop in quick proto {protocol} from ! <{LOOPBACK}> port {{ {gaps} }} to any"
            );
        }
    }
}

fn port_range(first: u32, last: u32) -> String {
    if first == last {
        first.to_string()
    } else {
        format!("{first}:{last}")
    }
}

fn loopback_addresses() -> BTreeSet<String> {
    LOOPBACK_ADDRESSES.into_iter().map(str::to_owned).collect()
}

fn numbers(values: impl Iterator<Item = u8>) -> String {
    values
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}
