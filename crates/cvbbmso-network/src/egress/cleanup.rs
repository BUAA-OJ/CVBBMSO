//! The macOS launchd fallback retries recovery after a crash or an expired lease.

use crate::NetworkError;
use crate::egress::backend::macos::HELPER;
use crate::egress::commands::{Commands, Diagnostic};

const TASK: &str = "com.cvbbmso.egress-cleanup";
const DOMAIN: &str = "system/com.cvbbmso.egress-cleanup";
const DEFINITION: &str = "/Library/LaunchDaemons/com.cvbbmso.egress-cleanup.plist";
const LAUNCHCTL: &str = "/bin/launchctl";
const PENDING: &str = "/Library/Application Support/com.cvbbmso/egress-cleanup.pending";
const ABSENT: &str = "Could not find service \"com.cvbbmso.egress-cleanup\" in domain for system";

pub(crate) fn schedule(commands: &dyn Commands) -> Result<(), NetworkError> {
    commands.write(DEFINITION, &render_definition())?;
    commands.run(LAUNCHCTL, &["bootstrap", "system", DEFINITION])?;
    verify(commands)
}

pub(crate) fn verify(commands: &dyn Commands) -> Result<(), NetworkError> {
    commands.run(LAUNCHCTL, &["print", DOMAIN])
}

pub(crate) fn cancel(commands: &dyn Commands) -> Result<(), NetworkError> {
    commands.write(PENDING, TASK)?;
    // Delete the definition before bootout, which may stop the recovery helper
    // that is calling this function.
    commands.remove(DEFINITION)?;
    if let Err(error) = commands.run(LAUNCHCTL, &["bootout", DOMAIN]) {
        // A timed-out launchctl may have already handed the request to
        // launchd. Keep the failure visible instead of treating it as absence.
        if error != NetworkError::Platform {
            return Err(error);
        }
        match commands.diagnostic(LAUNCHCTL, &["print", DOMAIN])? {
            Diagnostic::Rejected(message) if message.lines().any(|line| line.trim() == ABSENT) => {}
            _ => return Err(error),
        }
    }
    commands.remove(PENDING)
}

fn render_definition() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key><string>{TASK}</string>\n\
  <key>ProgramArguments</key><array><string>{HELPER}</string><string>--recover</string></array>\n\
  <key>StartInterval</key><integer>60</integer>\n\
</dict>\n\
</plist>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::commands::testing::Recorder;

    #[test]
    fn launchd_uses_the_fixed_privileged_helper() {
        let recorder = Recorder::default();
        schedule(&recorder).unwrap();
        let job = recorder.contents(DEFINITION).unwrap();
        assert!(job.contains(HELPER));
        assert!(job.contains("--recover"));
        cancel(&recorder).unwrap();
        assert!(recorder.contents(DEFINITION).is_none());
    }

    #[test]
    fn a_timed_out_launchctl_is_not_evidence_the_job_is_gone() {
        let recorder = Recorder::default();
        recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Io));
        assert_eq!(cancel(&recorder), Err(NetworkError::Io));
        assert_eq!(recorder.contents(PENDING).as_deref(), Some(TASK));

        recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Platform));
        recorder.reply(&["print", DOMAIN], Err(NetworkError::Io));
        assert_eq!(cancel(&recorder), Err(NetworkError::Io));
        assert_eq!(recorder.contents(PENDING).as_deref(), Some(TASK));
    }

    #[test]
    fn a_failed_bootout_clears_pending_only_after_confirmed_absence() {
        let recorder = Recorder::default();
        recorder.rejected(&["bootout", DOMAIN], "Boot-out failed");
        recorder.rejected(&["print", DOMAIN], "Permission denied");
        assert_eq!(cancel(&recorder), Err(NetworkError::Platform));
        assert_eq!(recorder.contents(PENDING).as_deref(), Some(TASK));

        recorder.rejected(&["bootout", DOMAIN], "Boot-out failed");
        recorder.rejected(&["print", DOMAIN], ABSENT);
        assert_eq!(cancel(&recorder), Ok(()));
        assert!(recorder.contents(PENDING).is_none());
    }
}
