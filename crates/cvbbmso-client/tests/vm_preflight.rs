//! Cross-platform tests for virtualization evidence, deadlines and admission.

#[path = "../src/preflight.rs"]
mod preflight;

mod admission {
    use std::future::ready;

    use super::preflight::{Diagnostic, Preflight, Verdict};

    #[tokio::test]
    async fn admission_waits_for_the_result_and_retries_only_unknown() {
        let preflight = Preflight::new();
        assert!(!preflight.is_admitted());
        let failure = Verdict::Unknown(Diagnostic::TimedOut);
        let passed = Verdict::NotDetected(Diagnostic::HardwareIdentity);
        let initial = preflight.schedule(false, ready(failure)).unwrap();
        assert!(!preflight.is_admitted());
        assert!(preflight.schedule(true, ready(passed)).is_none());
        initial.await;
        assert_eq!(preflight.verdict().await, failure);
        assert!(!preflight.is_admitted());
        assert!(preflight.schedule(false, ready(passed)).is_none());

        preflight.schedule(true, ready(passed)).unwrap().await;
        assert_eq!(preflight.verdict().await, passed);
        assert!(preflight.is_admitted());
        assert!(preflight.schedule(true, ready(failure)).is_none());
    }

    #[tokio::test]
    async fn detected_virtual_machines_cannot_retry_to_bypass_admission() {
        let preflight = Preflight::new();
        let detected = Verdict::Detected(Diagnostic::GuestIdentity);
        preflight.schedule(false, ready(detected)).unwrap().await;
        assert!(!preflight.is_admitted());
        let passed = Verdict::NotDetected(Diagnostic::HardwareIdentity);
        assert!(preflight.schedule(true, ready(passed)).is_none());
        assert_eq!(preflight.verdict().await, detected);
    }
}

mod command {
    use super::preflight::{
        Diagnostic,
        command::{read_probe, within_deadline},
    };
    use tokio::process::Command;

    #[tokio::test(start_paused = true)]
    async fn a_probe_that_never_completes_times_out() {
        let result = within_deadline(std::future::pending::<Result<(), Diagnostic>>()).await;
        assert_eq!(result, Err(Diagnostic::TimedOut));
    }

    #[tokio::test]
    async fn a_missing_platform_reader_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new(dir.path().join("missing-probe.exe"));
        assert_eq!(
            read_probe(&mut command).await,
            Err(Diagnostic::ProbeUnavailable)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_platform_reader_with_a_nonzero_exit_fails() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 1"]);
        assert_eq!(read_probe(&mut command).await, Err(Diagnostic::ProbeFailed));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_platform_reader_with_excess_output_is_rejected() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '%4097s' ''"]);
        assert_eq!(
            read_probe(&mut command).await,
            Err(Diagnostic::InvalidEvidence)
        );
    }
}

mod evidence {
    #[cfg(not(windows))]
    use super::preflight::evidence::macos_verdict;
    #[cfg(not(target_os = "macos"))]
    use super::preflight::evidence::windows_verdict;
    use super::preflight::{Diagnostic, Verdict};

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_virtualization_enabled_on_physical_hardware_is_allowed() {
        for hypervisor in [Some(true), Some(false), None] {
            assert!(matches!(
                windows_verdict(
                    Some("Microsoft Corporation"),
                    Some("Surface Laptop 7th Edition"),
                    hypervisor
                ),
                Verdict::NotDetected(_)
            ));
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_requires_usable_matching_guest_identity() {
        assert_eq!(
            windows_verdict(
                Some("microsoft Corporation "),
                Some("Virtual Machine"),
                Some(true)
            ),
            Verdict::Detected(Diagnostic::GuestIdentity)
        );
        assert_eq!(
            windows_verdict(
                Some(" VMware, Inc. "),
                Some(" VMware Virtual Platform "),
                Some(false)
            ),
            Verdict::Detected(Diagnostic::GuestIdentity)
        );
        assert_eq!(
            windows_verdict(Some("Dell Inc."), Some("Virtual Machine"), Some(true)),
            Verdict::Unknown(Diagnostic::ConflictingEvidence)
        );
        assert_eq!(
            windows_verdict(Some("QEMU"), None, Some(true)),
            Verdict::Unknown(Diagnostic::InvalidEvidence)
        );
        assert_eq!(
            windows_verdict(Some("To Be Filled By O.E.M."), Some("Desktop"), Some(true)),
            Verdict::Unknown(Diagnostic::PlaceholderIdentity)
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_parallels_arm_matches_the_reported_model_and_versioned_vendor() {
        // Windows 11 Pro build 22623, Parallels Desktop 18.1.1 device report.
        let vendor = Some("Parallels International GmbH version 18.1.1 (53328)");
        assert_eq!(
            windows_verdict(vendor, Some("Parallels ARM Virtual Machine"), None),
            Verdict::Detected(Diagnostic::GuestIdentity)
        );
        assert_eq!(
            windows_verdict(vendor, Some("Unfamiliar model"), None),
            Verdict::Unknown(Diagnostic::UnrecognizedGuestIdentity)
        );
        assert_eq!(
            windows_verdict(
                Some("Dell Inc."),
                Some("Parallels ARM Virtual Machine"),
                Some(true)
            ),
            Verdict::Unknown(Diagnostic::ConflictingEvidence)
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn windows_oem_placeholders_need_more_evidence_even_with_hyper_v() {
        for hypervisor in [Some(true), Some(false), None] {
            assert_eq!(
                windows_verdict(
                    Some("System manufacturer"),
                    Some("System Product Name"),
                    hypervisor
                ),
                Verdict::Unknown(Diagnostic::PlaceholderIdentity)
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn macos_checks_model_and_guest_indicator_together() {
        assert_eq!(
            macos_verdict("Mac16,12", "0"),
            Verdict::NotDetected(Diagnostic::HardwareIdentity)
        );
        assert_eq!(
            macos_verdict("VirtualMac2,1", "1"),
            Verdict::Detected(Diagnostic::GuestIdentity)
        );
        assert_eq!(
            macos_verdict("Mac16,12", "1"),
            Verdict::Unknown(Diagnostic::ConflictingEvidence)
        );
        assert_eq!(
            macos_verdict("VirtualMac2,1", "0"),
            Verdict::Unknown(Diagnostic::ConflictingEvidence)
        );
        assert_eq!(
            macos_verdict("Mac16,12", ""),
            Verdict::Unknown(Diagnostic::InvalidEvidence)
        );
        assert_eq!(
            macos_verdict("VirtualMac", "1"),
            Verdict::Unknown(Diagnostic::InvalidEvidence)
        );
    }
}
