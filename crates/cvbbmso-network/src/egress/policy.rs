//! Egress policy: validation and the rule set derived from it.

use std::net::{IpAddr, SocketAddr};

use url::Url;

use crate::NetworkError;

/// Origins the egress guard keeps reachable.
///
/// Both origins must be `https` origins with a nonempty host; the guard derives
/// the allowed port from the origin, so an explicit port is honoured and the
/// default is 443.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    /// Origin of the Accoding student site used by the controlled browser.
    pub accoding_origin: Url,
    /// Origin of the monitoring server, shared by the HTTP API and the WebSocket.
    pub server_origin: Url,
}

impl EgressPolicy {
    /// Validates the origins and reduces them to what the rule set needs.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError::Policy`] when an origin is not `https`, has no
    /// host, or has no derivable port.
    pub(crate) fn validate(&self) -> Result<ValidatedPolicy, NetworkError> {
        Ok(ValidatedPolicy {
            accoding: Endpoint::from_url(&self.accoding_origin)?,
            server: Endpoint::from_url(&self.server_origin)?,
        })
    }
}

/// One validated origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedPolicy {
    pub(crate) accoding: Endpoint,
    pub(crate) server: Endpoint,
}

/// An origin reduced to the host to resolve and the port to allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Endpoint {
    fn from_url(url: &Url) -> Result<Self, NetworkError> {
        if url.scheme() != "https" {
            return Err(NetworkError::Policy);
        }
        let Some(host) = url.host() else {
            return Err(NetworkError::Policy);
        };
        let host = match host {
            url::Host::Ipv6(ip) => ip.to_string(),
            other => other.to_string(),
        };
        if host.is_empty() {
            return Err(NetworkError::Policy);
        }
        let port = url.port_or_known_default().ok_or(NetworkError::Policy)?;
        Ok(Self { host, port })
    }
}

/// The addresses and ports one origin contributes to the rule set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) addresses: Vec<IpAddr>,
    pub(crate) port: u16,
}

impl Target {
    pub(crate) fn new(addresses: Vec<IpAddr>, port: u16) -> Self {
        Self { addresses, port }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Transport {
    #[cfg(any(windows, target_os = "macos"))]
    Tcp,
    Udp,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Service {
    pub(crate) remote: SocketAddr,
    pub(crate) transport: Transport,
}

impl Service {
    pub(crate) fn new(address: IpAddr, port: u16, transport: Transport) -> Self {
        Self {
            remote: SocketAddr::new(address, port),
            transport,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) accoding: Target,
    pub(crate) server: Target,
    pub(crate) certificate: Vec<SocketAddr>,
    pub(crate) services: Vec<Service>,
}

impl Plan {
    pub(crate) fn new(
        accoding: Target,
        server: Target,
        certificate: Vec<SocketAddr>,
        services: Vec<Service>,
    ) -> Self {
        Self {
            accoding,
            server,
            certificate,
            services,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EgressPolicy;
    use crate::NetworkError;
    use url::Url;

    fn policy(accoding: &str, server: &str) -> EgressPolicy {
        EgressPolicy {
            accoding_origin: Url::parse(accoding).expect("test URL"),
            server_origin: Url::parse(server).expect("test URL"),
        }
    }

    #[test]
    fn https_origins_are_validated() {
        let validated = policy(
            "https://accoding.buaa.edu.cn",
            "https://monitor.example.com",
        )
        .validate()
        .expect("valid policy");
        assert_eq!(validated.accoding.host, "accoding.buaa.edu.cn");
        assert_eq!(validated.accoding.port, 443);
        assert_eq!(validated.server.host, "monitor.example.com");
        assert_eq!(validated.server.port, 443);
    }

    #[test]
    fn an_explicit_port_is_kept() {
        let validated = policy(
            "https://accoding.buaa.edu.cn:8443",
            "https://monitor.example.com",
        )
        .validate()
        .expect("valid policy");
        assert_eq!(validated.accoding.port, 8443);
    }

    #[test]
    fn a_plain_http_origin_is_rejected() {
        assert_eq!(
            policy("http://accoding.buaa.edu.cn", "https://monitor.example.com")
                .validate()
                .expect_err("http must be rejected"),
            NetworkError::Policy
        );
    }

    #[test]
    fn a_missing_host_is_rejected() {
        assert_eq!(
            policy("https://monitor.example.com", "file:///tmp/x")
                .validate()
                .expect_err("non-HTTP scheme must be rejected"),
            NetworkError::Policy
        );
    }
}
