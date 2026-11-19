use std::{future::Future, process::Stdio, time::Duration};

use tokio::{io::AsyncReadExt, process::Command, time::timeout};

use super::Diagnostic;

const MAX_OUTPUT: u64 = 4096;

pub(crate) async fn read_probe(
    command: &mut Command,
    deadline: Duration,
) -> Result<Vec<u8>, Diagnostic> {
    within_deadline(deadline, async {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Diagnostic::ProbeUnavailable)?;
        let stdout = child.stdout.take().ok_or(Diagnostic::ProbeFailed)?;
        let mut bytes = Vec::new();
        stdout
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| Diagnostic::ProbeFailed)?;
        if bytes.len() as u64 > MAX_OUTPUT {
            return Err(Diagnostic::InvalidEvidence);
        }
        let status = child.wait().await.map_err(|_| Diagnostic::ProbeFailed)?;
        if !status.success() {
            return Err(Diagnostic::ProbeFailed);
        }
        Ok(bytes)
    })
    .await
}

pub(crate) async fn within_deadline<T>(
    deadline: Duration,
    probe: impl Future<Output = Result<T, Diagnostic>>,
) -> Result<T, Diagnostic> {
    timeout(deadline, probe)
        .await
        .map_err(|_| Diagnostic::TimedOut)?
}
