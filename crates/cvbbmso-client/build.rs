//! Generates the Windows/macOS application manifest and capability schemas.

#[cfg(any(target_os = "macos", windows))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = tauri_build::AppManifest::new().commands(&["open_accoding", "inspect_session"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {}
