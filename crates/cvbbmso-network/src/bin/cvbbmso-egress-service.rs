//! Entry point for the `LaunchDaemon` bundled in the signed macOS application.

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    return cvbbmso_network::macos_egress_service_main();
    #[cfg(not(target_os = "macos"))]
    std::process::ExitCode::FAILURE
}
