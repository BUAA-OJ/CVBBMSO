//! Desktop entry point for the CVBBMSO student application.

#![cfg_attr(
    all(windows, not(debug_assertions), not(test)),
    windows_subsystem = "windows"
)]

#[cfg(any(target_os = "macos", windows))]
mod desktop;
#[cfg(any(target_os = "macos", windows))]
mod policy;

#[cfg(any(target_os = "macos", windows))]
fn main() -> tauri::Result<()> {
    desktop::run()
}

#[cfg(all(not(any(target_os = "macos", windows)), not(test)))]
fn main() {
    eprintln!("CVBBMSO Desktop requires Windows or macOS.");
    std::process::exit(1);
}

#[cfg(all(not(any(target_os = "macos", windows)), test))]
fn main() {}
