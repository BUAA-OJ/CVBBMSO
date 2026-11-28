//! The macOS launchd fallback retries recovery after a crash or an expired lease.

use crate::NetworkError;
use crate::egress::commands::{Commands, Diagnostic};
use crate::egress::state::Generation;

const TASK: &str = "com.cvbbmso.egress-cleanup";
const LEASE_PREFIX: &str = "com.cvbbmso.egress-cleanup.v2.";
const LAUNCHCTL: &str = "/bin/launchctl";
const PENDING: &str = "/Library/Application Support/com.cvbbmso/egress-cleanup.pending";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Job {
    Legacy,
    Lease(Generation),
}

impl Job {
    pub(crate) fn new(generation: Option<Generation>) -> Self {
        generation.map_or(Self::Legacy, Self::Lease)
    }

    fn label(&self) -> String {
        match self {
            Self::Legacy => TASK.to_owned(),
            Self::Lease(generation) => format!("{LEASE_PREFIX}{}", generation.as_str()),
        }
    }

    fn definition(&self) -> String {
        format!("/Library/LaunchDaemons/{}.plist", self.label())
    }

    fn domain(&self) -> String {
        format!("system/{}", self.label())
    }

    pub(crate) fn schedule(
        commands: &dyn Commands,
        generation: &Generation,
        helper: &str,
    ) -> Result<(), NetworkError> {
        let job = Self::Lease(generation.clone());
        let definition = job.definition();
        let contents = job.render_definition(helper, generation);
        commands.write_new(&definition, &contents)?;
        commands.run(LAUNCHCTL, &["bootstrap", "system", &definition])?;
        job.verify(commands)
    }

    pub(crate) fn verify(&self, commands: &dyn Commands) -> Result<(), NetworkError> {
        commands.run(LAUNCHCTL, &["print", &self.domain()])
    }

    pub(crate) fn pending(commands: &dyn Commands) -> Result<Option<Self>, NetworkError> {
        let value = commands.read(PENDING)?;
        match value.as_deref() {
            None => Ok(None),
            Some(TASK) => Ok(Some(Self::Legacy)),
            Some(label) => label
                .strip_prefix(LEASE_PREFIX)
                .ok_or(NetworkError::Io)
                .and_then(|name| {
                    Generation::try_from(name.to_owned()).map_err(|_| NetworkError::Io)
                })
                .map(|generation| Some(Self::Lease(generation))),
        }
    }

    pub(crate) fn record_cleanup(&self, commands: &dyn Commands) -> Result<(), NetworkError> {
        if Self::pending(commands)?.is_some_and(|pending| pending != *self) {
            return Err(NetworkError::Io);
        }
        commands.write(PENDING, &self.label())
    }

    pub(crate) fn finish_cleanup(&self, commands: &dyn Commands) -> Result<(), NetworkError> {
        if Self::pending(commands)?.as_ref() != Some(self) {
            return Err(NetworkError::Io);
        }
        self.unload(commands)?;
        commands.remove(PENDING)
    }

    /// Only touches this label and definition. A retired lease may call it
    /// after a newer lease has acquired the PF anchor.
    pub(crate) fn unload(&self, commands: &dyn Commands) -> Result<(), NetworkError> {
        // Remove the definition before bootout, which may also stop the caller.
        commands.remove(&self.definition())?;
        let domain = self.domain();
        if let Err(error) = commands.run(LAUNCHCTL, &["bootout", &domain]) {
            if error != NetworkError::Platform {
                return Err(error);
            }
            let absent = format!(
                "Could not find service \"{}\" in domain for system",
                self.label()
            );
            match commands.diagnostic(LAUNCHCTL, &["print", &domain])? {
                Diagnostic::Rejected(message)
                    if message.lines().any(|line| line.trim() == absent) => {}
                _ => return Err(error),
            }
        }
        Ok(())
    }

    fn render_definition(&self, helper: &str, generation: &Generation) -> String {
        let arguments = format!(
            "<string>--recover-lease</string><string>{}</string>",
            generation.as_str()
        );
        let label = self.label();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key><array><string>{helper}</string>{arguments}</array>\n\
  <key>StartInterval</key><integer>60</integer>\n\
</dict>\n\
</plist>\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::commands::testing::Recorder;

    const DOMAIN: &str = "system/com.cvbbmso.egress-cleanup";
    const ABSENT: &str =
        "Could not find service \"com.cvbbmso.egress-cleanup\" in domain for system";

    #[test]
    fn a_timed_out_launchctl_is_not_evidence_the_job_is_gone() {
        let recorder = Recorder::default();
        let job = Job::Legacy;
        job.record_cleanup(&recorder).unwrap();
        recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Io));
        assert_eq!(job.finish_cleanup(&recorder), Err(NetworkError::Io));
        assert_eq!(Job::pending(&recorder).unwrap(), Some(Job::Legacy));

        recorder.reply(&["bootout", DOMAIN], Err(NetworkError::Platform));
        recorder.reply(&["print", DOMAIN], Err(NetworkError::Io));
        assert_eq!(job.finish_cleanup(&recorder), Err(NetworkError::Io));
        assert_eq!(Job::pending(&recorder).unwrap(), Some(Job::Legacy));
    }

    #[test]
    fn a_failed_bootout_clears_pending_only_after_confirmed_absence() {
        let recorder = Recorder::default();
        let job = Job::Legacy;
        job.record_cleanup(&recorder).unwrap();
        recorder.rejected(&["bootout", DOMAIN], "Boot-out failed");
        recorder.rejected(&["print", DOMAIN], "Permission denied");
        assert_eq!(job.finish_cleanup(&recorder), Err(NetworkError::Platform));
        assert_eq!(Job::pending(&recorder).unwrap(), Some(Job::Legacy));

        recorder.rejected(&["bootout", DOMAIN], "Boot-out failed");
        recorder.rejected(&["print", DOMAIN], ABSENT);
        assert_eq!(job.finish_cleanup(&recorder), Ok(()));
        assert_eq!(Job::pending(&recorder).unwrap(), None);
    }

    #[test]
    fn lease_definitions_are_distinct_and_cannot_be_overwritten() {
        let recorder = Recorder::default();
        let generation = Generation::allocate(&recorder).unwrap();
        let lease = Job::Lease(generation.clone());
        let next = Job::Lease(Generation::allocate(&recorder).unwrap());
        let helper = crate::egress::session::HELPER;
        Job::schedule(&recorder, &generation, helper).unwrap();
        let definition = recorder.contents(&lease.definition()).unwrap();
        assert!(definition.contains(&format!("<string>{helper}</string>")));
        assert!(definition.contains("<string>--recover-lease</string>"));
        assert!(definition.contains(generation.as_str()));
        assert_eq!(
            Job::schedule(&recorder, &generation, helper),
            Err(NetworkError::Io)
        );
        assert_ne!(lease.definition(), next.definition());
        assert_ne!(lease.domain(), next.domain());
    }
}
