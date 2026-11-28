//! Tests the macOS authorization command without asking for root privileges.

#![cfg(target_os = "macos")]

use std::process::{Command, Output};

const AUTHORIZATION: &str = include_str!("../src/desktop/macos/authorize.applescript");
const INSTALLER: &str = include_str!("../src/desktop/macos/install.sh");

fn succeeded(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn authorization_quotes_the_install_arguments_and_clears_the_root_environment() {
    let helper = AUTHORIZATION.split_once("\non run argv").unwrap().0;
    // Exercise the production command builder without its authorization entry.
    assert!(!helper.to_ascii_lowercase().contains("do shell script"));
    let preview = format!("{helper}\non run argv\nreturn installCommand(argv)\nend run");
    let source = "/Volumes/考试 O'Reilly $(printf substituted)/CVBBMSO.app";
    let cdhash = "0123456789abcdef0123456789abcdef01234567";
    let command = succeeded(
        Command::new("/usr/bin/osascript")
            .args([
                "-e",
                &preview,
                "--",
                r#"printf '%s\n' "$1" "$2" "${CVBBMSO_TEST_UNTRUSTED-unset}""#,
                source,
                cdhash,
            ])
            .output()
            .unwrap(),
    );
    let command = String::from_utf8(command).unwrap();
    let output = succeeded(
        Command::new("/bin/sh")
            .env("CVBBMSO_TEST_UNTRUSTED", "inherited")
            .args(["-c", &command])
            .output()
            .unwrap(),
    );
    assert_eq!(output, format!("{source}\n{cdhash}\nunset\n").as_bytes());
}

#[test]
fn the_installer_checks_privilege_before_touching_the_installation() {
    let uid = succeeded(Command::new("/usr/bin/id").arg("-u").output().unwrap());
    assert_ne!(uid, b"0\n", "run authorization tests as an ordinary user");
    succeeded(
        Command::new("/bin/bash")
            .args(["-n", "-c", INSTALLER])
            .output()
            .unwrap(),
    );
    let output = Command::new("/bin/bash")
        .args([
            "-p",
            "-c",
            INSTALLER,
            "cvbbmso-install",
            "/tmp/CVBBMSO.app",
            "0123456789abcdef0123456789abcdef01234567",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        output.stderr,
        b"Invalid application installation request.\n"
    );
}
