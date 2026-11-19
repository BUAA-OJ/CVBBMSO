use tokio::process::Command;

use crate::preflight::{Diagnostic, Verdict, command::read_probe, evidence::macos_verdict};

pub(crate) async fn detect() -> Verdict {
    let mut command = Command::new("/usr/sbin/sysctl");
    command.args(["-n", "hw.model", "kern.hv_vmm_present"]);
    let output = match read_probe(&mut command).await {
        Ok(output) => output,
        Err(error) => return Verdict::Unknown(error),
    };
    if let Ok(text) = std::str::from_utf8(&output) {
        let mut values = text.lines();
        if let (Some(model), Some(guest), None) = (values.next(), values.next(), values.next()) {
            return macos_verdict(model, guest);
        }
    }

    Verdict::Unknown(Diagnostic::InvalidEvidence)
}
