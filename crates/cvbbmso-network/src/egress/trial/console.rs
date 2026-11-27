//! Trusted process launch and peer credentials for the trial socket.
#![allow(
    unsafe_code,
    reason = "Darwin supplies process and Unix peer credentials"
)]

use std::ffi::CStr;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::process::Child;

use crate::NetworkError;
use crate::egress::state;

use super::HELPER;

const APP: &str = "/Library/Application Support/com.cvbbmso-trial/CVBBMSO Trial.app/Contents/MacOS/cvbbmso-client";

pub(super) fn protected(path: &Path, directory: bool) -> Result<(), NetworkError> {
    for ancestor in path.ancestors().skip(1) {
        state::check_protected(ancestor, true)?;
    }
    state::check_protected(path, directory)
}

pub(super) fn check_app() -> Result<(), NetworkError> {
    protected(Path::new(APP), false)?;
    let executable = std::fs::symlink_metadata(APP).map_err(|_| NetworkError::Io)?;
    if executable.mode() & 0o6000 != 0 {
        return Err(NetworkError::MacosUntrustedInstallation);
    }
    Ok(())
}

pub(super) fn console_uid() -> Result<libc::uid_t, NetworkError> {
    let uid = std::fs::metadata("/dev/console")
        .map_err(|_| NetworkError::Io)?
        .uid();
    if uid < 501 {
        return Err(NetworkError::PermissionDenied);
    }
    Ok(uid)
}

pub(super) fn peer_uid(socket: &UnixStream) -> Result<libc::uid_t, NetworkError> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: the socket is live and the output pointers refer to writable values.
    if unsafe { libc::getpeereid(socket.as_raw_fd(), &raw mut uid, &raw mut gid) } == 0 {
        Ok(uid)
    } else {
        Err(NetworkError::MacosServiceUnavailable)
    }
}

fn peer_pid(socket: RawFd) -> Result<libc::pid_t, NetworkError> {
    let mut pid: libc::pid_t = 0;
    let mut size =
        libc::socklen_t::try_from(size_of::<libc::pid_t>()).map_err(|_| NetworkError::Platform)?;
    let expected = size;
    // SAFETY: the output buffer and its length are valid for a LOCAL_PEERPID query.
    if unsafe {
        libc::getsockopt(
            socket,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &raw mut size,
        )
    } == 0
        && size == expected
        && pid > 1
    {
        Ok(pid)
    } else {
        Err(NetworkError::MacosServiceUnavailable)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Process {
    pid: libc::pid_t,
    started: u64,
    micros: u64,
}

impl Process {
    fn inspect(pid: libc::pid_t) -> Result<Self, NetworkError> {
        // SAFETY: proc_bsdinfo contains only integer fields and fixed byte arrays.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let length =
            i32::try_from(size_of::<libc::proc_bsdinfo>()).map_err(|_| NetworkError::Platform)?;
        // SAFETY: info is writable and has the size expected by PROC_PIDTBSDINFO.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                length,
            )
        };
        if read != length || pid <= 1 || info.pbi_start_tvsec == 0 {
            return Err(NetworkError::MacosServiceUnavailable);
        }
        Ok(Self {
            pid,
            started: info.pbi_start_tvsec,
            micros: info.pbi_start_tvusec,
        })
    }

    pub(super) fn matches_peer(&self, stream: &UnixStream, uid: libc::uid_t) -> bool {
        peer_uid(stream).ok() == Some(uid)
            && peer_pid(stream.as_raw_fd()).ok() == Some(self.pid)
            && Self::inspect(self.pid).as_ref().ok() == Some(self)
            && is_trial_app(self.pid)
    }
}

fn is_trial_app(pid: libc::pid_t) -> bool {
    let mut path = [0_u8; 4096];
    let Ok(length) = u32::try_from(path.len()) else {
        return false;
    };
    // SAFETY: path is a writable buffer large enough for proc_pidpath.
    if unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), length) } <= 0 {
        return false;
    }
    CStr::from_bytes_until_nul(&path).is_ok_and(|path| path.to_bytes() == APP.as_bytes())
}

pub(super) async fn spawn(uid: libc::uid_t) -> Result<(Child, Process), NetworkError> {
    // asuser adopts the GUI bootstrap context. The protected launch process
    // drops credentials itself and reports the PID it will exec into.
    let uid = uid.to_string();
    let mut child = tokio::process::Command::new("/bin/launchctl")
        .args(["asuser", &uid, HELPER, "--trial-launch-client", &uid])
        .env_clear()
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| NetworkError::MacosServiceUnavailable)?;
    let mut output = child.stdout.take().ok_or(NetworkError::Platform)?;
    let identity = Process {
        pid: output
            .read_i32()
            .await
            .map_err(|_| NetworkError::MacosServiceUnavailable)?,
        started: output
            .read_u64()
            .await
            .map_err(|_| NetworkError::MacosServiceUnavailable)?,
        micros: output
            .read_u64()
            .await
            .map_err(|_| NetworkError::MacosServiceUnavailable)?,
    };
    if identity.pid <= 1 || identity.started == 0 {
        return Err(NetworkError::MacosServiceUnavailable);
    }
    Ok((child, identity))
}

pub(super) fn exec_app(expected_uid: libc::uid_t) -> Result<(), NetworkError> {
    check_app()?;
    let uid = console_uid()?;
    if uid != expected_uid {
        return Err(NetworkError::PermissionDenied);
    }
    // SAFETY: this fresh launch process is single-threaded. Use the account
    // name returned by getpwuid only before any further password database call.
    let account = unsafe { libc::getpwuid(uid).as_ref() }.ok_or(NetworkError::PermissionDenied)?;
    if account.pw_name.is_null() || account.pw_gid == 0 {
        return Err(NetworkError::PermissionDenied);
    }
    let gid = account.pw_gid;
    let base_group = i32::try_from(gid).map_err(|_| NetworkError::PermissionDenied)?;
    let mut output = std::io::stdout().lock();
    let pid = i32::try_from(std::process::id()).map_err(|_| NetworkError::Platform)?;
    let identity = Process::inspect(pid)?;
    output
        .write_all(&identity.pid.to_be_bytes())
        .and_then(|()| output.write_all(&identity.started.to_be_bytes()))
        .and_then(|()| output.write_all(&identity.micros.to_be_bytes()))
        .and_then(|()| output.flush())
        .map_err(|_| NetworkError::Io)?;
    drop(output);
    // SAFETY: the account name is NUL-terminated and comes from the system
    // account database. Drop supplementary groups and root before exec.
    if unsafe { libc::initgroups(account.pw_name, base_group) } != 0
        || unsafe { libc::setgid(gid) } != 0
        || unsafe { libc::setuid(uid) } != 0
        || unsafe { libc::getuid() != uid || libc::geteuid() != uid }
        || unsafe { libc::getgid() != gid || libc::getegid() != gid }
    {
        return Err(NetworkError::PermissionDenied);
    }
    let error = Command::new(APP)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .exec();
    eprintln!("Could not launch the trial App: {error}");
    Err(NetworkError::MacosServiceUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_connecting_uid_and_pid_do_not_make_an_unrelated_executable_the_trial_app() {
        let (socket, _) = UnixStream::pair().unwrap();
        let uid = peer_uid(&socket).unwrap();
        let pid = i32::try_from(std::process::id()).unwrap();
        let process = Process::inspect(pid).unwrap();
        assert!(!process.matches_peer(&socket, uid));
    }
}
