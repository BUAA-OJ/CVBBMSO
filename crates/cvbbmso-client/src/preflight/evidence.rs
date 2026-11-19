use super::{Diagnostic, Verdict};

#[cfg(not(target_os = "macos"))]
pub(crate) fn windows_verdict(
    manufacturer: Option<&str>,
    model: Option<&str>,
    hypervisor_present: Option<bool>,
) -> Verdict {
    let usable = |value: &str| {
        !matches!(
            value,
            "" | "unknown"
                | "none"
                | "not applicable"
                | "to be filled by o.e.m."
                | "default string"
                | "system manufacturer"
                | "system product name"
        )
    };
    let normalized = |value: &str| {
        value
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase()
    };
    let (Some(manufacturer), Some(model)) = (manufacturer.map(normalized), model.map(normalized))
    else {
        return Verdict::Unknown(Diagnostic::InvalidEvidence);
    };
    let vendor = manufacturer.as_str();
    let is_parallels = [
        "parallels international gmbh",
        "parallels software international inc.",
    ]
    .iter()
    .any(|name| {
        vendor == *name
            || vendor
                .strip_prefix(name)
                .and_then(|suffix| suffix.strip_prefix(" version "))
                .and_then(|version| version.bytes().next())
                .is_some_and(|first| first.is_ascii_digit())
    });
    // Strong evidence is a guest-model family corroborated by its expected vendor.
    // A hypervisor flag is also present on physical Windows hosts using VBS.
    let guest_vendor_matches = match model.as_str() {
        "virtual machine" => Some(vendor == "microsoft corporation"),
        "vmware virtual platform" | "vmware7,1" => Some(vendor == "vmware, inc."),
        "virtualbox" => Some(matches!(vendor, "innotek gmbh" | "oracle corporation")),
        "parallels virtual platform" | "parallels arm virtual machine" => Some(is_parallels),
        "kvm virtual machine" | "qemu virtual machine" | "qemu arm virtual machine" => {
            Some(matches!(vendor, "qemu" | "red hat"))
        }
        model if model.starts_with("standard pc (") => Some(vendor == "qemu"),
        _ => None,
    };
    match guest_vendor_matches {
        Some(true) => return Verdict::Detected(Diagnostic::GuestIdentity),
        Some(false) => return Verdict::Unknown(Diagnostic::ConflictingEvidence),
        None => {}
    }
    if is_parallels || matches!(vendor, "vmware, inc." | "innotek gmbh" | "qemu") {
        return Verdict::Unknown(Diagnostic::UnrecognizedGuestIdentity);
    }
    if !usable(vendor) || !usable(&model) {
        return Verdict::Unknown(Diagnostic::PlaceholderIdentity);
    }

    Verdict::NotDetected(if hypervisor_present == Some(true) {
        Diagnostic::HypervisorPresent
    } else {
        Diagnostic::HardwareIdentity
    })
}

#[cfg(not(windows))]
pub(crate) fn macos_verdict(model: &str, guest: &str) -> Verdict {
    let model = model.trim();
    let Some((family, version)) = model
        .find(|c: char| c.is_ascii_digit())
        .map(|i| model.split_at(i))
    else {
        return Verdict::Unknown(Diagnostic::InvalidEvidence);
    };
    let number =
        |value: &str| !value.is_empty() && value.bytes().all(|digit| digit.is_ascii_digit());
    if !version
        .split_once(',')
        .is_some_and(|(major, minor)| number(major) && number(minor))
    {
        return Verdict::Unknown(Diagnostic::InvalidEvidence);
    }
    match (family, guest.trim()) {
        ("VirtualMac", "1") => Verdict::Detected(Diagnostic::GuestIdentity),
        (
            "Mac" | "MacBook" | "MacBookAir" | "MacBookPro" | "Macmini" | "MacPro" | "iMac"
            | "iMacPro",
            "0",
        ) => Verdict::NotDetected(Diagnostic::HardwareIdentity),
        ("VirtualMac", "0") | (_, "1") => Verdict::Unknown(Diagnostic::ConflictingEvidence),
        _ => Verdict::Unknown(Diagnostic::InvalidEvidence),
    }
}
