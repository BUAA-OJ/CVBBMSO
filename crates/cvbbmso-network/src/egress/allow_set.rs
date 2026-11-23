//! Allowed addresses, with a grace period for replaced entries.

use std::collections::HashSet;
use std::hash::Hash;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::NetworkError;

/// Addresses of one origin, keeping replaced addresses reachable for a while so
/// connections that already started are not cut.
#[derive(Debug, Clone)]
struct EndpointSet<T> {
    current: HashSet<T>,
    retired: Vec<(T, Instant)>,
}

impl<T> Default for EndpointSet<T> {
    fn default() -> Self {
        Self {
            current: HashSet::new(),
            retired: Vec::new(),
        }
    }
}

impl<T: Copy + Eq + Hash + Ord> EndpointSet<T> {
    fn replace(&mut self, next: Vec<T>, now: Instant, grace: Duration) {
        let next: HashSet<T> = next.into_iter().collect();
        for address in self.current.difference(&next) {
            self.retired.push((*address, now + grace));
        }
        self.retired
            .retain(|(address, until)| *until > now && !next.contains(address));
        self.current = next;
    }

    fn is_empty(&self) -> bool {
        self.current.is_empty()
    }

    fn active(&self, now: Instant) -> Vec<T> {
        let mut addresses: Vec<T> = self
            .current
            .iter()
            .copied()
            .chain(
                self.retired
                    .iter()
                    .filter(|(_, until)| *until > now)
                    .map(|(address, _)| *address),
            )
            .collect();
        addresses.sort();
        addresses.dedup();
        addresses
    }
}

/// Addresses the guard allows, split by purpose.
#[derive(Debug, Clone)]
pub(crate) struct AllowSet {
    accoding: EndpointSet<IpAddr>,
    server: EndpointSet<IpAddr>,
    certificate: EndpointSet<SocketAddr>,
    authentication: EndpointSet<SocketAddr>,
    grace: Duration,
}

impl AllowSet {
    pub(crate) fn new(grace: Duration) -> Self {
        Self {
            accoding: EndpointSet::default(),
            server: EndpointSet::default(),
            certificate: EndpointSet::default(),
            authentication: EndpointSet::default(),
            grace,
        }
    }

    /// Applies a new resolution result.
    ///
    /// An empty list keeps the previous addresses for that origin, so a
    /// temporary resolution failure does not break a running exam.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError::Resolution`] when neither origin resolved and no
    /// previous set exists.
    pub(crate) fn refresh(
        &mut self,
        accoding: Vec<IpAddr>,
        server: Vec<IpAddr>,
        certificate: Vec<SocketAddr>,
        now: Instant,
    ) -> Result<(), NetworkError> {
        let nothing_known = self.accoding.is_empty() && self.server.is_empty();
        if accoding.is_empty() && server.is_empty() && nothing_known {
            return Err(NetworkError::Resolution);
        }
        if !accoding.is_empty() {
            self.accoding.replace(accoding, now, self.grace);
        }
        if !server.is_empty() {
            self.server.replace(server, now, self.grace);
        }
        self.certificate.replace(certificate, now, self.grace);
        Ok(())
    }

    /// Returns the addresses to allow, in rule-set order.
    pub(crate) fn active(&self, now: Instant) -> (Vec<IpAddr>, Vec<IpAddr>, Vec<SocketAddr>) {
        (
            self.accoding.active(now),
            self.server.active(now),
            self.certificate.active(now),
        )
    }

    pub(crate) fn refresh_authentication(&mut self, next: Vec<SocketAddr>, now: Instant) {
        if !next.is_empty() {
            self.authentication.replace(next, now, self.grace);
        }
    }

    pub(crate) fn authentication(&self, now: Instant) -> Vec<SocketAddr> {
        self.authentication.active(now)
    }
}

#[cfg(test)]
mod tests {
    use super::AllowSet;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};

    fn address(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
    }

    #[test]
    fn campus_authentication_survives_dns_failure_and_rotates_with_grace() {
        let grace = Duration::from_secs(120);
        let mut set = AllowSet::new(grace);
        let now = Instant::now();
        let old = std::net::SocketAddr::new(address(1), 443);
        let new = std::net::SocketAddr::new(address(2), 443);
        set.refresh_authentication(vec![old], now);
        set.refresh_authentication(Vec::new(), now);
        assert_eq!(set.authentication(now), [old]);
        set.refresh_authentication(vec![new], now);
        assert_eq!(set.authentication(now), [old, new]);
        assert_eq!(set.authentication(now + grace), [new]);
    }

    fn populated(grace: Duration) -> (AllowSet, Instant) {
        let now = Instant::now();
        let mut set = AllowSet::new(grace);
        set.refresh(vec![address(1)], vec![address(2)], Vec::new(), now)
            .expect("first refresh succeeds");
        (set, now)
    }

    #[test]
    fn first_refresh_without_addresses_is_a_resolution_failure() {
        let mut set = AllowSet::new(Duration::from_secs(60));
        assert_eq!(
            set.refresh(Vec::new(), Vec::new(), Vec::new(), Instant::now())
                .expect_err("no addresses"),
            crate::NetworkError::Resolution
        );
    }

    #[test]
    fn a_missing_origin_keeps_its_previous_addresses() {
        let grace = Duration::from_secs(60);
        let (mut set, now) = populated(grace);
        set.refresh(Vec::new(), vec![address(3)], Vec::new(), now)
            .expect("server resolved");

        // The unresolved origin keeps its address; the replaced one stays active
        // for the grace period so running connections are not cut.
        let (accoding, server, _) = set.active(now);
        assert_eq!(accoding, vec![address(1)]);
        assert_eq!(server, vec![address(2), address(3)]);

        let (_, server, _) = set.active(now + grace);
        assert_eq!(server, vec![address(3)]);
    }

    #[test]
    fn a_replaced_address_stays_active_during_the_grace_period() {
        let grace = Duration::from_secs(60);
        let (mut set, now) = populated(grace);
        set.refresh(vec![address(9)], vec![address(2)], Vec::new(), now)
            .expect("refresh succeeds");
        assert_eq!(set.active(now).0, vec![address(1), address(9)]);
        assert_eq!(set.active(now + grace).0, vec![address(9)]);
    }
}
