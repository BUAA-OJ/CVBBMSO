use std::{ffi::c_void, fmt::Write as _, io, path::Path};

const AUTHORIZATION: &str = include_str!("authorize.applescript");
const INSTALLER: &str = include_str!("install.sh");
const CS_OPS_CDHASH: u32 = 5;

unsafe extern "C" {
    fn csops(pid: libc::pid_t, ops: libc::c_uint, user_addr: *mut c_void, size: usize) -> i32;
}

#[derive(Debug, thiserror::Error)]
pub(in crate::desktop) enum AuthorizationError {
    #[error("the running executable is not inside a CVBBMSO app bundle")]
    Bundle,
    #[error("unable to prepare or request system authorization")]
    Io(#[from] io::Error),
    #[error("system authorization was cancelled")]
    Cancelled,
    #[error("the administrator operation failed")]
    Failed,
}

fn running_cdhash() -> io::Result<String> {
    let mut hash = [0_u8; 20];
    // SAFETY: CS_OPS_CDHASH for our PID writes a 20-byte code directory hash
    // into this writable buffer. It identifies the loaded executable even
    // if the source bundle has since been replaced on disk.
    let result = unsafe {
        csops(
            libc::getpid(),
            CS_OPS_CDHASH,
            hash.as_mut_ptr().cast(),
            hash.len(),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut encoded = String::with_capacity(hash.len() * 2);
    for byte in hash {
        write!(encoded, "{byte:02x}").map_err(io::Error::other)?;
    }
    Ok(encoded)
}

pub(in crate::desktop) async fn launch() -> Result<(), AuthorizationError> {
    let executable = std::env::current_exe()?;
    let bundle = executable
        .ancestors()
        .nth(3)
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "app")
                && executable.strip_prefix(path).ok()
                    == Some(Path::new("Contents/MacOS/cvbbmso-client"))
        })
        .ok_or(AuthorizationError::Bundle)?;
    let output = tokio::process::Command::new("/usr/bin/osascript")
        .kill_on_drop(true)
        .arg("-e")
        .arg(AUTHORIZATION)
        .arg("--")
        .arg(INSTALLER)
        .arg(bundle)
        .arg(running_cdhash()?)
        .output()
        .await?;
    if output.status.success() {
        return Ok(());
    }
    if String::from_utf8_lossy(&output.stderr)
        .trim_end()
        .ends_with("(-128)")
    {
        return Err(AuthorizationError::Cancelled);
    }
    Err(AuthorizationError::Failed)
}
