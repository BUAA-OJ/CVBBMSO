//! Administrator-launched helper for macOS PF.

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "macos")]
    return cvbbmso_network::macos_pf_main();
    #[cfg(not(target_os = "macos"))]
    ExitCode::FAILURE
}
