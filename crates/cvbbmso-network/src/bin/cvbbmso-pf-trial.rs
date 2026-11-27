//! Administrator-launched helper for the macOS PF trial.

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "macos")]
    return cvbbmso_network::macos_pf_trial_main();
    #[cfg(not(target_os = "macos"))]
    ExitCode::FAILURE
}
