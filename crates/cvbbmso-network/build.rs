//! Builds the native macOS egress bridge for Cargo's target architecture.

use std::env;
use std::path::Path;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=CVBBMSO_MACOS_TEAM_ID");
    println!("cargo:rerun-if-env-changed=CVBBMSO_SERVER_ORIGIN");
    println!("cargo:rerun-if-changed=native/macos_egress.m");
    if env::var("CARGO_CFG_TARGET_OS")?.as_str() != "macos" {
        return Ok(());
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH")?.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        _ => return Err("unsupported macOS architecture".into()),
    };
    let out = env::var("OUT_DIR")?;
    let object = Path::new(&out).join("macos_egress.o");
    let archive = Path::new(&out).join("libmacos_egress.a");
    let compiled = Command::new("/usr/bin/xcrun")
        .args([
            "--sdk",
            "macosx",
            "clang",
            "-arch",
            arch,
            "-mmacosx-version-min=11.0",
            "-fobjc-arc",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-c",
            "native/macos_egress.m",
            "-o",
        ])
        .arg(&object)
        .status()?;
    if !compiled.success() {
        return Err("could not compile macOS egress bridge".into());
    }
    if !Command::new("/usr/bin/ar")
        .arg("rcs")
        .arg(&archive)
        .arg(&object)
        .status()?
        .success()
    {
        return Err("could not archive macOS egress bridge".into());
    }
    println!("cargo:rustc-link-search=native={out}");
    println!("cargo:rustc-link-lib=static=macos_egress");
    for framework in ["Foundation", "Security", "ServiceManagement"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    Ok(())
}
