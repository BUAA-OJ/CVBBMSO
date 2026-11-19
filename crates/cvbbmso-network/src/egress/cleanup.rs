//! One-shot OS cleanup task that reverts a guard left behind by a crash.
//!
//! The task starts the watchdog helper in recovery mode on a fixed interval. The
//! helper does nothing while the recorded guard lifetime is still valid, so the
//! fallback never tears down an active guard.

use std::path::Path;
use std::time::Duration;

use crate::NetworkError;
use crate::egress::commands::{Commands, SystemCommands};

/// Task label used by the platform scheduler.
#[cfg(any(target_os = "macos", windows))]
const TASK_NAME: &str = "com.cvbbmso.egress-cleanup";

/// Registers the fallback task.
///
/// # Errors
///
/// Returns [`NetworkError::Platform`] or [`NetworkError::Io`] when the task
/// cannot be registered.
pub(crate) fn schedule(helper: &Path, lifetime: Duration) -> Result<(), NetworkError> {
    schedule_with(&SystemCommands, helper, lifetime)
}

/// Removes the fallback task. Succeeds when no task is registered.
///
/// # Errors
///
/// Returns [`NetworkError::Io`] when the task definition cannot be removed.
pub(crate) fn cancel() -> Result<(), NetworkError> {
    cancel_with(&SystemCommands)
}

#[cfg(target_os = "macos")]
fn definition_path() -> &'static str {
    "/Library/LaunchDaemons/com.cvbbmso.egress-cleanup.plist"
}

#[cfg(target_os = "macos")]
fn schedule_with(
    commands: &dyn Commands,
    helper: &Path,
    lifetime: Duration,
) -> Result<(), NetworkError> {
    let interval = lifetime.as_secs().max(60);
    commands.write(definition_path(), &render_definition(helper, interval))?;
    commands.run(
        "/bin/launchctl",
        &["bootstrap", "system", definition_path()],
    )
}

#[cfg(target_os = "macos")]
fn cancel_with(commands: &dyn Commands) -> Result<(), NetworkError> {
    let target = format!("system/{TASK_NAME}");
    // Unloading a task that is not loaded is not an error for the guard.
    let _ = commands.run("/bin/launchctl", &["bootout", &target]);
    commands.remove(definition_path())
}

/// Renders the launchd job that runs the helper in recovery mode.
#[cfg(target_os = "macos")]
fn render_definition(helper: &Path, interval: u64) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key>\n\
  <string>{TASK_NAME}</string>\n\
  <key>ProgramArguments</key>\n\
  <array>\n\
    <string>{helper}</string>\n\
    <string>--recover</string>\n\
  </array>\n\
  <key>StartInterval</key>\n\
  <integer>{interval}</integer>\n\
  <key>RunAtLoad</key>\n\
  <false/>\n\
</dict>\n\
</plist>\n",
        helper = helper.display()
    )
}

#[cfg(windows)]
fn schedule_with(
    commands: &dyn Commands,
    helper: &Path,
    _lifetime: Duration,
) -> Result<(), NetworkError> {
    let command = format!("{} --recover", helper.display());
    commands.run(
        "schtasks",
        &[
            "/Create",
            "/F",
            "/SC",
            "MINUTE",
            "/MO",
            "15",
            "/TN",
            TASK_NAME,
            "/TR",
            command.as_str(),
        ],
    )
}

#[cfg(windows)]
fn cancel_with(commands: &dyn Commands) -> Result<(), NetworkError> {
    // Deleting a task that is not registered is not an error for the guard.
    let _ = commands.run("schtasks", &["/Delete", "/F", "/TN", TASK_NAME]);
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn schedule_with(
    _commands: &dyn Commands,
    _helper: &Path,
    _lifetime: Duration,
) -> Result<(), NetworkError> {
    Err(NetworkError::Platform)
}

#[cfg(not(any(target_os = "macos", windows)))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature is shared with the platform adapters"
)]
fn cancel_with(_commands: &dyn Commands) -> Result<(), NetworkError> {
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{cancel_with, definition_path, render_definition, schedule_with};
    use crate::egress::commands::testing::{Call, Recorder};
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn the_job_runs_the_helper_in_recovery_mode() {
        let recorder = Recorder::default();
        schedule_with(
            &recorder,
            Path::new("/usr/local/bin/watchdog"),
            Duration::from_secs(900),
        )
        .expect("schedule succeeds");

        let definition = recorder.contents(definition_path()).expect("plist written");
        assert!(definition.contains("<string>/usr/local/bin/watchdog</string>"));
        assert!(definition.contains("<string>--recover</string>"));
        assert!(definition.contains("<integer>900</integer>"));

        assert!(
            recorder
                .calls()
                .iter()
                .any(|call| matches!(call, Call::Run(program, _) if program == "/bin/launchctl"))
        );
    }

    #[test]
    fn cancelling_removes_the_job_definition() {
        let recorder = Recorder::default();
        schedule_with(
            &recorder,
            Path::new("/usr/local/bin/watchdog"),
            Duration::from_secs(900),
        )
        .expect("schedule succeeds");
        cancel_with(&recorder).expect("cancel succeeds");

        assert!(recorder.contents(definition_path()).is_none());
    }

    #[test]
    fn the_rendered_job_is_a_plist() {
        let rendered = render_definition(Path::new("/tmp/watchdog"), 900);
        assert!(rendered.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(rendered.contains("<key>Label</key>"));
    }
}
