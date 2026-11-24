//! System egress guard.
//!
//! Installing the guard restricts outbound traffic to the Accoding origin, the
//! monitoring server and the services an exam client needs; the restriction is
//! enforced by the platform filter adapter and kept alive for as long as the
//! application runs. Installation requires administrator or root rights and every
//! step rolls back when a later step fails.

mod allow_set;
mod backend;
#[cfg(target_os = "macos")]
mod cleanup;
#[cfg(target_os = "macos")]
mod commands;
#[cfg(target_os = "macos")]
pub(crate) mod managed;
mod policy;
mod resolver;
mod services;
#[cfg(target_os = "macos")]
mod state;

use std::future::{Future, poll_fn};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use self::allow_set::AllowSet;
use self::backend::GuardBackend;
use self::policy::{Plan, Service, Target, Transport, ValidatedPolicy};
use self::resolver::{Resolver, SystemResolver};
use self::services::Services;
use crate::NetworkError;

pub use self::policy::EgressPolicy;

/// Campus network authentication host, allowed only over HTTPS (TCP port 443).
const CAMPUS_AUTH_HOST: &str = "gw.buaa.edu.cn";

/// How long an address that disappeared from DNS stays allowed.
const ADDRESS_GRACE: Duration = Duration::from_secs(120);
/// Interval at which the allowed addresses are resolved again.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
// https://support.apple.com/101555 and the Windows Root Certificate Program
// specify these endpoints and ports for their respective trust stores.
#[cfg(target_os = "macos")]
const CERTIFICATE_HOSTS: &[(&str, u16)] = &[
    ("certs.apple.com", 80),
    ("certs.apple.com", 443),
    ("crl.apple.com", 80),
    ("crl3.digicert.com", 80),
    ("crl4.digicert.com", 80),
    ("ocsp.apple.com", 80),
    ("ocsp.digicert.cn", 80),
    ("ocsp.digicert.com", 80),
    ("ocsp2.apple.com", 443),
    ("valid.apple.com", 443),
];
#[cfg(windows)]
const CERTIFICATE_HOSTS: &[(&str, u16)] = &[("ctldl.windowsupdate.com", 80)];
#[cfg(not(any(windows, target_os = "macos")))]
const CERTIFICATE_HOSTS: &[(&str, u16)] = &[];

/// Active system-level egress guard.
///
/// Use [`EgressGuard::shutdown`] to remove the filter when the app exits.
pub struct EgressGuard {
    backend: Arc<dyn GuardBackend>,
    endpoints: ValidatedPolicy,
    runtime: Arc<Mutex<Runtime>>,
}

struct Runtime {
    allowed: AllowSet,
    config: Services,
    last_refresh: Option<Instant>,
}

impl EgressGuard {
    /// Installs the system filter for `policy`.
    ///
    /// # Errors
    ///
    /// [`NetworkError::Policy`] when an origin is not a usable `https` origin,
    /// [`NetworkError::Resolution`] when an origin currently has no address,
    /// [`NetworkError::PermissionDenied`] without administrator or root rights,
    /// and [`NetworkError::Platform`] or [`NetworkError::Io`] when the platform
    /// filter or its supporting files cannot be installed.
    pub async fn install(policy: EgressPolicy) -> Result<Self, NetworkError> {
        Self::install_using(
            policy,
            backend::platform::prepare,
            backend::platform::install,
        )
        .await
    }

    async fn install_using(
        policy: EgressPolicy,
        prepare: fn() -> Result<(), NetworkError>,
        install: fn(&Plan) -> Result<backend::InstallResult, NetworkError>,
    ) -> Result<Self, NetworkError> {
        let endpoints = policy.validate()?;
        let config = tokio::task::spawn_blocking(move || {
            prepare()?;
            Services::discover()
        })
        .await
        .map_err(|_| NetworkError::Platform)??;

        let resolved = resolve_endpoints(&SystemResolver, &endpoints).await;
        if resolved.accoding.is_empty() || resolved.server.is_empty() {
            return Err(NetworkError::Resolution);
        }
        let now = Instant::now();
        let mut allowed = AllowSet::new(ADDRESS_GRACE);
        allowed.refresh(
            resolved.accoding,
            resolved.server,
            resolved.certificate,
            now,
        )?;
        allowed.refresh_authentication(resolved.authentication, now);
        let destinations = config.resolve(&SystemResolver).await;
        let plan = make_plan(&endpoints, &allowed, destinations, now);
        let backend = tokio::task::spawn_blocking(move || install(&plan))
            .await
            .map_err(|_| NetworkError::Platform)??;
        Ok(Self {
            backend,
            endpoints,
            runtime: Arc::new(Mutex::new(Runtime {
                allowed,
                config,
                last_refresh: Some(now),
            })),
        })
    }

    /// Confirms that the filter is still installed and refreshes the allowed
    /// addresses when they are due.
    ///
    /// # Errors
    ///
    /// Returns [`NetworkError::Platform`] when the rules disappeared, or a
    /// platform, I/O or resolution error when verification cannot complete.
    pub async fn verify(&self) -> Result<(), NetworkError> {
        let backend = Arc::clone(&self.backend);
        let endpoints = self.endpoints.clone();
        let runtime = Arc::clone(&self.runtime);
        let refresh = tokio::task::spawn_blocking(move || {
            let mut runtime = runtime.lock().map_err(|_| NetworkError::Platform)?;
            backend.verify()?;
            let config = Services::discover()?;
            if runtime.config != config {
                // The new resolver must be reachable before DNS queries can
                // refresh the origins after a network change.
                let plan = make_plan(
                    &endpoints,
                    &runtime.allowed,
                    config.rules().to_vec(),
                    Instant::now(),
                );
                backend.apply(&plan)?;
                runtime.config = config;
                runtime.last_refresh = None;
            }
            Ok::<_, NetworkError>(runtime.refresh_due().then(|| runtime.config.clone()))
        })
        .await
        .map_err(|_| NetworkError::Platform)??;
        let Some(config) = refresh else {
            return Ok(());
        };

        let (resolved, destinations) = tokio::join!(
            resolve_endpoints(&SystemResolver, &self.endpoints),
            config.resolve(&SystemResolver)
        );
        let backend = Arc::clone(&self.backend);
        let endpoints = self.endpoints.clone();
        let runtime = Arc::clone(&self.runtime);
        tokio::task::spawn_blocking(move || {
            let mut runtime = runtime.lock().map_err(|_| NetworkError::Platform)?;
            if runtime.config == config && runtime.refresh_due() {
                runtime.refresh(
                    backend.as_ref(),
                    &endpoints,
                    resolved,
                    destinations,
                    Instant::now(),
                )?;
            }
            Ok(())
        })
        .await
        .map_err(|_| NetworkError::Platform)?
    }

    /// Removes the filter and its platform cleanup resources.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform could not complete cleanup.
    pub async fn shutdown(self) -> Result<(), NetworkError> {
        self.shutdown_inner().await
    }

    async fn shutdown_inner(&self) -> Result<(), NetworkError> {
        let backend = Arc::clone(&self.backend);
        tokio::task::spawn_blocking(move || backend.revert())
            .await
            .map_err(|_| NetworkError::Platform)?
    }
}

impl Runtime {
    fn refresh_due(&self) -> bool {
        self.last_refresh
            .is_none_or(|last| last.elapsed() >= REFRESH_INTERVAL)
    }

    fn refresh(
        &mut self,
        backend: &dyn GuardBackend,
        endpoints: &ValidatedPolicy,
        resolved: Resolved,
        destinations: Vec<Service>,
        now: Instant,
    ) -> Result<(), NetworkError> {
        let mut candidate = self.allowed.clone();
        candidate.refresh(
            resolved.accoding,
            resolved.server,
            resolved.certificate,
            now,
        )?;
        candidate.refresh_authentication(resolved.authentication, now);
        backend.apply(&make_plan(endpoints, &candidate, destinations, now))?;
        self.allowed = candidate;
        self.last_refresh = Some(now);
        Ok(())
    }
}

pub(crate) fn watchdog_entry() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    return backend::platform::watchdog_entry();
    #[cfg(not(target_os = "macos"))]
    std::process::ExitCode::FAILURE
}

/// Resolved addresses of everything the policy allows.
struct Resolved {
    accoding: Vec<IpAddr>,
    server: Vec<IpAddr>,
    certificate: Vec<SocketAddr>,
    authentication: Vec<SocketAddr>,
}

async fn resolve_endpoints(resolver: &impl Resolver, endpoints: &ValidatedPolicy) -> Resolved {
    let (accoding, server, certificate, authentication) = tokio::join!(
        resolver.resolve(&endpoints.accoding.host, endpoints.accoding.port),
        resolver.resolve(&endpoints.server.host, endpoints.server.port),
        resolve_certificate(resolver),
        resolver.resolve(CAMPUS_AUTH_HOST, 443),
    );
    Resolved {
        authentication: authentication
            .unwrap_or_default()
            .into_iter()
            .filter(|address| services::is_unicast(*address))
            .map(|address| SocketAddr::new(address, 443))
            .collect(),
        accoding: accoding.unwrap_or_default(),
        server: server.unwrap_or_default(),
        certificate,
    }
}

async fn resolve_certificate(resolver: &impl Resolver) -> Vec<SocketAddr> {
    let mut certificate = Vec::new();
    let mut pending = CERTIFICATE_HOSTS
        .iter()
        .map(|&(host, port)| (port, Box::pin(resolver.resolve(host, port))))
        .collect::<Vec<_>>();
    // Slow auxiliary names must not serialize the individual DNS timeouts.
    poll_fn(|context| {
        pending.retain_mut(|(port, lookup)| match lookup.as_mut().poll(context) {
            Poll::Ready(result) => {
                if let Ok(addresses) = result {
                    certificate.extend(
                        addresses
                            .into_iter()
                            .map(|address| SocketAddr::new(address, *port)),
                    );
                }
                false
            }
            Poll::Pending => true,
        });
        if pending.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    certificate.sort_unstable();
    certificate.dedup();
    certificate
}

fn make_plan(
    endpoints: &ValidatedPolicy,
    allowed: &AllowSet,
    mut services: Vec<Service>,
    now: Instant,
) -> Plan {
    services.extend(
        allowed
            .authentication(now)
            .into_iter()
            .map(|remote| Service::new(remote.ip(), remote.port(), Transport::Tcp)),
    );
    services.sort_unstable();
    services.dedup();
    let (accoding, server, certificate) = allowed.active(now);
    Plan::new(
        Target::new(accoding, endpoints.accoding.port),
        Target::new(server, endpoints.server.port),
        certificate,
        services,
    )
}

#[cfg(test)]
mod tests {
    use super::{ADDRESS_GRACE, AllowSet, make_plan};
    use crate::egress::policy::EgressPolicy;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Instant;
    use url::Url;

    fn policy() -> EgressPolicy {
        EgressPolicy {
            accoding_origin: Url::parse("https://accoding.buaa.edu.cn").expect("test URL"),
            server_origin: Url::parse("https://monitor.example.com").expect("test URL"),
        }
    }

    #[test]
    fn a_plan_carries_both_ports() {
        let endpoints = policy().validate().expect("valid policy");
        let mut allowed = AllowSet::new(ADDRESS_GRACE);
        let address = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
        allowed
            .refresh(vec![address], vec![address], Vec::new(), Instant::now())
            .expect("refresh succeeds");

        let plan = make_plan(&endpoints, &allowed, Vec::new(), Instant::now());
        assert_eq!(plan.accoding.port, 443);
        assert_eq!(plan.server.port, 443);
        assert_eq!(plan.accoding.addresses, vec![address]);
        assert_eq!(plan.server.addresses, vec![address]);
    }

    #[test]
    fn campus_authentication_adds_only_resolved_https_tcp_destinations() {
        let endpoints = policy().validate().unwrap();
        let now = Instant::now();
        let mut allowed = AllowSet::new(ADDRESS_GRACE);
        let remote = "192.0.2.78:443".parse().unwrap();
        allowed.refresh_authentication(vec![remote], now);
        let plan = make_plan(&endpoints, &allowed, Vec::new(), now);
        assert_eq!(
            plan.services,
            [super::Service::new(remote.ip(), 443, super::Transport::Tcp)]
        );
        assert_eq!(plan.accoding.addresses, [] as [IpAddr; 0]);
        assert_eq!(plan.server.addresses, [] as [IpAddr; 0]);
    }

    struct CampusResolver;

    impl super::Resolver for CampusResolver {
        fn resolve(
            &self,
            host: &str,
            port: u16,
        ) -> impl std::future::Future<Output = Result<Vec<IpAddr>, crate::NetworkError>> {
            let result = if host == super::CAMPUS_AUTH_HOST {
                assert_eq!(port, 443);
                Ok(vec!["192.0.2.78".parse().unwrap(), "::".parse().unwrap()])
            } else {
                Err(crate::NetworkError::Resolution)
            };
            std::future::ready(result)
        }
    }

    #[tokio::test]
    async fn campus_authentication_resolution_is_independent_of_monitor_dns() {
        let resolved =
            super::resolve_endpoints(&CampusResolver, &policy().validate().unwrap()).await;
        assert_eq!(resolved.authentication, ["192.0.2.78:443".parse().unwrap()]);
        assert_eq!(resolved.accoding, [] as [IpAddr; 0]);
        assert_eq!(resolved.server, [] as [IpAddr; 0]);
    }

    struct RefusingBackend;

    impl super::GuardBackend for RefusingBackend {
        fn apply(&self, _plan: &super::Plan) -> Result<(), crate::NetworkError> {
            Err(crate::NetworkError::Platform)
        }

        fn verify(&self) -> Result<(), crate::NetworkError> {
            Ok(())
        }

        fn revert(&self) -> Result<(), crate::NetworkError> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_refresh_leaves_the_installed_address_snapshot_due_for_retry() {
        use super::{Resolved, Runtime, Services};
        let endpoints = policy().validate().unwrap();
        let now = Instant::now();
        let old = "192.0.2.1".parse::<IpAddr>().unwrap();
        let new = "192.0.2.2".parse::<IpAddr>().unwrap();
        let mut allowed = AllowSet::new(ADDRESS_GRACE);
        allowed
            .refresh(vec![old], vec![old], Vec::new(), now)
            .unwrap();
        let mut runtime = Runtime {
            allowed,
            config: Services::default(),
            last_refresh: None,
        };
        assert_eq!(
            runtime.refresh(
                &RefusingBackend,
                &endpoints,
                Resolved {
                    authentication: Vec::new(),
                    accoding: vec![new],
                    server: vec![new],
                    certificate: Vec::new(),
                },
                Vec::new(),
                now
            ),
            Err(crate::NetworkError::Platform)
        );
        assert_eq!(runtime.allowed.active(now).0, vec![old]);
        assert!(runtime.refresh_due());
    }
}
