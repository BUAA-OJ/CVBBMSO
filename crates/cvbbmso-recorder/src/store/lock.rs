//! Exclusive ownership of one recording directory across processes.

use std::{fs::File, fs::OpenOptions, io, path::Path};

pub(super) fn open(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.share_mode(0);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    unix::try_lock(&file)?;
    Ok(file)
}

#[cfg(unix)]
mod unix {
    use std::{ffi::c_int, fs::File, io, os::fd::AsRawFd as _};

    // File::try_lock was stabilized after the workspace's Rust 1.88 MSRV.
    unsafe extern "C" {
        fn flock(fd: c_int, operation: c_int) -> c_int;
    }

    pub(super) fn try_lock(file: &File) -> io::Result<()> {
        const LOCK_EX: c_int = 2;
        const LOCK_NB: c_int = 4;
        // SAFETY: the file owns a live descriptor; these flags request an
        // exclusive nonblocking lock which is released by closing the file.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
