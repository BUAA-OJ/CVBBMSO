//! The macOS launchd fallback retries recovery after a crash or an expired lease.

use crate::NetworkError;
use crate::egress::backend::macos::HELPER;
use crate::egress::commands::Commands;

const TASK: &str = "com.cvbbmso.egress-cleanup";
const DOMAIN: &str = "system/com.cvbbmso.egress-cleanup";
const DEFINITION: &str = "/Library/LaunchDaemons/com.cvbbmso.egress-cleanup.plist";
const LAUNCHCTL: &str = "/bin/launchctl";

pub(crate) fn schedule(commands: &dyn Commands) -> Result<(), NetworkError> {
    commands.write(DEFINITION, &render_definition())?;
    commands.run(LAUNCHCTL, &["bootstrap", "system", DEFINITION])?;
    verify(commands)
}

pub(crate) fn verify(commands: &dyn Commands) -> Result<(), NetworkError> {
    commands.run(LAUNCHCTL, &["print", DOMAIN])
}

pub(crate) fn cancel(commands: &dyn Commands) -> Result<(), NetworkError> {
    // Delete the definition before bootout, which may stop the recovery helper
    // that is calling this function.
    commands.remove(DEFINITION)?;
    if commands.run(LAUNCHCTL, &["bootout", DOMAIN]).is_err()
        && commands.run(LAUNCHCTL, &["print", DOMAIN]).is_ok()
    {
        return Err(NetworkError::Platform);
    }
    Ok(())
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
}
