//! Watchdog helper for the system egress guard.
//!
//! This binary is started by `EgressGuard::install` and by the one-shot OS
//! cleanup task. It watches the client process and the recorded guard lifetime,
//! then reverts the platform filter. The binary is a thin wrapper; the logic
//! lives in the library so it can be tested without privilege.

fn main() -> std::process::ExitCode {
    cvbbmso_network::watchdog_main()
}
