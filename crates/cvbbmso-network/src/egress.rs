//! System egress guard.
//!
//! Installing the guard restricts outbound traffic to the Accoding origin, the
//! monitoring server and the services an exam client needs; the restriction is
//! enforced by the platform filter adapter and kept alive for as long as the
//! application runs. Installation requires administrator or root rights and every
//! step rolls back when a later step fails.

mod allow_set;
mod backend;
mod cleanup;
mod commands;
mod policy;
mod resolver;
mod state;
mod watchdog;

use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use self::allow_set::AllowSet;
use self::backend::GuardBackend;
use self::policy::{Plan, Target, ValidatedPolicy};
use self::resolver::{Resolver, SystemResolver};
use crate::NetworkError;

pub use self::policy::EgressPolicy;

/// How long an address that disappeared from DNS stays allowed.
const ADDRESS_GRACE: Duration = Duration::from_secs(120);
/// Guard lifetime without renewal; [`EgressGuard::verify`] renews it.
const GUARD_TTL: Duration = Duration::from_secs(15 * 60);
/// Interval at which the allowed addresses are resolved again.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Certificate status hosts the platform trust stack needs.
const CERTIFICATE_HOSTS: [&str; 3] = ["ocsp.apple.com", "ocsp2.apple.com", "crl.apple.com"];

/// Active system-level egress guard.
///
/// Dropping the guard without calling [`EgressGuard::shutdown`] leaves the rules
/// to the watchdog helper, which reverts them once this process or the recorded
/// lifetime ends.
pub struct EgressGuard {
    backend: std::sync::Arc<dyn GuardBackend>,
    watchdog: watchdog::Watchdog,
    endpoints: ValidatedPolicy,
    allowed: Mutex<AllowSet>,
    last_refresh: Mutex<Instant>,
    expires_at: Mutex<Instant>,
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
    /// filter or one of the supporting files cannot be installed. Every error
    /// leaves the system without guard rules.
    pub async fn install(policy: EgressPolicy) -> Result<Self, NetworkError> {
        let endpoints = policy.validate()?;
        remove_stale_state()?;

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
        let plan = make_plan(&endpoints, &allowed, now);

        // Registering the watchdog and the TTL task comes before the rules, so a
        // crash between the two steps still leaves cleanup behind.
        let state_path = state::path();
        let watchdog = watchdog::Watchdog::start(&state_path)?;
        let mut guard_state = state::GuardState::new(GUARD_TTL);
        guard_state.set_watchdog(watchdog.pid());
        if let Err(error) = state::write(&guard_state) {
            watchdog.stop();
            return Err(error);
        }
        if let Err(error) = schedule_cleanup().await {
            watchdog.stop();
            let _ = state::remove();
            return Err(error);
        }

        let backend = match apply_to_platform(plan).await {
            Ok(backend) => backend,
            Err(error) => {
                watchdog.stop();
                let _ = cleanup::cancel();
                let _ = state::remove();
                return Err(error);
            }
        };

        Ok(Self {
            backend,
            watchdog,
            endpoints,
            allowed: Mutex::new(allowed),
            last_refresh: Mutex::new(now),
            expires_at: Mutex::new(now + GUARD_TTL),
        })
    }

    /// Confirms that the filter is still installed, refreshes the allowed
    /// addresses when they are due, and renews the recorded lifetime.
    ///
    /// # Errors
    ///
    /// [`NetworkError::Platform`] when the rules disappeared, and the same
    /// errors as [`EgressGuard::install`] when addresses cannot be refreshed.
    pub async fn verify(&self) -> Result<(), NetworkError> {
        let backend = std::sync::Arc::clone(&self.backend);
        tokio::task::spawn_blocking(move || backend.verify())
            .await
            .map_err(|_| NetworkError::Platform)??;

        self.refresh_addresses().await?;
        self.renew_lifetime();
        Ok(())
    }

    /// Removes the filter, stops the watchdog and the TTL task.
    ///
    /// # Errors
    ///
    /// Returns the first failure of the individual removal steps; every step is
    /// attempted regardless.
    pub async fn shutdown(self) -> Result<(), NetworkError> {
        let backend = std::sync::Arc::clone(&self.backend);
        let reverted = tokio::task::spawn_blocking(move || backend.revert())
            .await
            .map_err(|_| NetworkError::Platform);
        self.watchdog.stop();
        let cleanup_removed = cleanup::cancel();
        let state_removed = state::remove();
        reverted??;
        cleanup_removed?;
        state_removed
    }

    /// Re-resolves the policy addresses when the refresh interval elapsed.
    async fn refresh_addresses(&self) -> Result<(), NetworkError> {
        let due = {
            let last = *self
                .last_refresh
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            last.elapsed() >= REFRESH_INTERVAL
        };
        if !due {
            return Ok(());
        }

        let resolved = resolve_endpoints(&SystemResolver, &self.endpoints).await;
        let now = Instant::now();
        let plan = {
            let mut allowed = self.allowed.lock().unwrap_or_else(PoisonError::into_inner);
            allowed.prune(now);
            allowed.refresh(
                resolved.accoding,
                resolved.server,
                resolved.certificate,
                now,
            )?;
            make_plan(&self.endpoints, &allowed, now)
        };
        *self
            .last_refresh
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = now;

        let backend = std::sync::Arc::clone(&self.backend);
        tokio::task::spawn_blocking(move || backend.apply(&plan))
            .await
            .map_err(|_| NetworkError::Platform)??;
        Ok(())
    }

    /// Rewrites the state file so the watchdog does not treat the guard as
    /// abandoned.
    fn renew_lifetime(&self) {
        let deadline = Instant::now() + GUARD_TTL;
        *self
            .expires_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = deadline;
        if let Some(mut state) = state::read() {
            state.renew(GUARD_TTL);
            state.set_watchdog(self.watchdog.pid());
            let _ = state::write(&state);
        }
    }
}

/// Runs the watchdog helper with the process command line.
pub(crate) fn watchdog_entry() -> std::process::ExitCode {
    watchdog::run_from_args(std::env::args().skip(1))
}

/// Removes state and the TTL task left behind by an earlier run.
fn remove_stale_state() -> Result<(), NetworkError> {
    let _ = cleanup::cancel();
    match state::read() {
        Some(_) => state::remove(),
        None => Ok(()),
    }
}

/// Applies `plan` on a blocking thread.
async fn apply_to_platform(plan: Plan) -> Result<std::sync::Arc<dyn GuardBackend>, NetworkError> {
    tokio::task::spawn_blocking(move || {
        // Leftover rules from an earlier run must go before new rules arrive.
        backend::platform::recover()?;
        backend::platform::install(&plan)
    })
    .await
    .map_err(|_| NetworkError::Platform)?
}

/// Registers the TTL fallback on a blocking thread.
async fn schedule_cleanup() -> Result<(), NetworkError> {
    tokio::task::spawn_blocking(|| {
        let helper = watchdog::helper_path()?;
        cleanup::schedule(&helper, GUARD_TTL)
    })
    .await
    .map_err(|_| NetworkError::Platform)?
}

/// Resolved addresses of everything the policy allows.
struct Resolved {
    accoding: Vec<IpAddr>,
    server: Vec<IpAddr>,
    certificate: Vec<IpAddr>,
}

/// Resolves both origins and the certificate status hosts.
async fn resolve_endpoints<R: Resolver>(resolver: &R, endpoints: &ValidatedPolicy) -> Resolved {
    let accoding = resolver
        .resolve(&endpoints.accoding.host, endpoints.accoding.port)
        .await
        .unwrap_or_default();
    let server = resolver
        .resolve(&endpoints.server.host, endpoints.server.port)
        .await
        .unwrap_or_default();
    let mut certificate = Vec::new();
    for host in CERTIFICATE_HOSTS {
        certificate.extend(resolver.resolve(host, 443).await.unwrap_or_default());
    }
    certificate.sort();
    certificate.dedup();
    Resolved {
        accoding,
        server,
        certificate,
    }
}

/// Builds the rule set from the current allowed addresses.
fn make_plan(endpoints: &ValidatedPolicy, allowed: &AllowSet, now: Instant) -> Plan {
    let (accoding, server, certificate) = allowed.active(now);
    Plan::new(
        Target::new(accoding, endpoints.accoding.port),
        Target::new(server, endpoints.server.port),
        certificate,
        system_resolvers(),
    )
}

/// Returns the addresses of the resolvers the system is configured to use.
#[cfg(unix)]
fn system_resolvers() -> Vec<IpAddr> {
    let Ok(contents) = std::fs::read_to_string("/etc/resolv.conf") else {
        return Vec::new();
    };
    let mut resolvers: Vec<IpAddr> = contents
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver "))
        .filter_map(|address| address.trim().parse().ok())
        .collect();
    resolvers.sort();
    resolvers.dedup();
    resolvers
}

/// Returns an empty list because Windows exposes resolvers through its own API.
#[cfg(not(unix))]
fn system_resolvers() -> Vec<IpAddr> {
    Vec::new()
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

        let plan = make_plan(&endpoints, &allowed, Instant::now());
        assert_eq!(plan.accoding.port, 443);
        assert_eq!(plan.server.port, 443);
        assert_eq!(plan.accoding.addresses, vec![address]);
        assert_eq!(plan.server.addresses, vec![address]);
    }
}
