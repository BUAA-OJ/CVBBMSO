//! Commands and files the platform adapters need.
//!
//! The trait exists as a test seam: the real implementation shells out to the
//! platform tools, tests record the calls instead of touching the firewall.
#![allow(
    unsafe_code,
    reason = "Darwin supplies child deadlines and process group termination"
)]

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::NetworkError;

pub(crate) const CAPTURE_PID: &str = "Capture PID: ";
pub(crate) const CAPTURE_PID_DIGITS: usize = 10;

/// Runs platform tools and reads or writes the files they need.
#[cfg_attr(
    not(any(target_os = "macos", windows)),
    allow(dead_code, reason = "only the platform adapters call these methods")
)]
pub(crate) trait Commands {
    /// Returns the wall clock used by the PF reference listing.
    fn unix_seconds(&self) -> Result<u64, NetworkError> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| NetworkError::Io)
    }

    /// Runs `program` and fails unless it exits successfully.
    fn run(&self, program: &str, args: &[&str]) -> Result<(), NetworkError>;

    /// Runs `program` and returns its standard output.
    fn output(&self, program: &str, args: &[&str]) -> Result<String, NetworkError>;

    /// Runs a command whose normal rejection includes a diagnostic on stderr.
    fn diagnostic(&self, program: &str, args: &[&str]) -> Result<Diagnostic, NetworkError>;

    /// Runs a command with its output written directly to a new journal.
    fn capture(&self, program: &str, args: &[&str], path: &str) -> Result<(), NetworkError>;

    /// Reads a file, or returns `None` when it does not exist.
    fn read(&self, path: &str) -> Result<Option<String>, NetworkError>;

    /// Returns the file's modification time in whole Unix seconds.
    fn modified_seconds(&self, path: &str) -> Result<u64, NetworkError>;

    /// Writes a file, replacing its contents.
    fn write(&self, path: &str, contents: &str) -> Result<(), NetworkError>;

    /// Creates a file atomically and refuses to replace an existing file.
    fn write_new(&self, path: &str, contents: &str) -> Result<(), NetworkError>;

    /// Publishes a complete journal and removes its staging name atomically.
    fn publish(&self, staging: &str, path: &str) -> Result<(), NetworkError>;

    /// Removes a file, succeeding when it does not exist.
    fn remove(&self, path: &str) -> Result<(), NetworkError>;
}

#[derive(Debug)]
pub(crate) enum Diagnostic {
    Success(String),
    Rejected(String),
}

impl Diagnostic {
    fn checked(self) -> Result<String, NetworkError> {
        match self {
            Self::Success(output) => Ok(output),
            Self::Rejected(_) => Err(NetworkError::Platform),
        }
    }
}

/// The inherited lock keeps recovery from overtaking an orphaned platform command.
pub(crate) struct SystemCommands<'a> {
    lock: Option<&'a File>,
    unsettled: Cell<bool>,
}

const COMMAND_TIMEOUT_SECONDS: u32 = 5;
pub(super) const REAP_TIMEOUT: Duration = Duration::from_secs(1);
const CHECK_INTERVAL: Duration = Duration::from_millis(15);

impl<'a> SystemCommands<'a> {
    pub(crate) fn new(lock: Option<&'a File>) -> Self {
        Self {
            lock,
            unsettled: Cell::new(false),
        }
    }

    fn ensure_settled(&self) -> Result<(), NetworkError> {
        if self.unsettled.get() {
            Err(NetworkError::Io)
        } else {
            Ok(())
        }
    }

    fn write_atomic(&self, path: &str, contents: &str, replace: bool) -> Result<(), NetworkError> {
        self.ensure_settled()?;
        let path = Path::new(path);
        let directory = path.parent().ok_or(NetworkError::Io)?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).map_err(|_| NetworkError::Io)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if replace && metadata.is_file() => {
                temporary
                    .as_file()
                    .set_permissions(metadata.permissions())
                    .map_err(|_| NetworkError::Io)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(NetworkError::Io),
        }
        temporary
            .write_all(contents.as_bytes())
            .map_err(|_| NetworkError::Io)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| NetworkError::Io)?;
        if replace {
            temporary.persist(path)
        } else {
            temporary.persist_noclobber(path)
        }
        .map_err(|_| NetworkError::Io)?;
        Ok(())
    }

    fn command(&self, program: &str, args: &[&str]) -> Result<Command, NetworkError> {
        self.ensure_settled()?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir("/")
            .env_clear()
            .env("LC_ALL", "C")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(lock) = self.lock {
            command.stdin(lock.try_clone().map_err(|_| NetworkError::Io)?);
        }
        // SAFETY: the hook runs in the forked child and calls only
        // async-signal-safe operations, with no allocation before exec.
        unsafe { command.pre_exec(arm_child_alarm) };
        Ok(command)
    }

    fn read_output(
        &self,
        program: &str,
        args: &[&str],
        include_error: bool,
    ) -> Result<Diagnostic, NetworkError> {
        // A file can be collected after the process has exited without
        // blocking on a pipe while the command owns the cross-process lock.
        let mut output = tempfile::tempfile().map_err(|_| NetworkError::Io)?;
        let mut command = self.command(program, args)?;
        command.stdout(output.try_clone().map_err(|_| NetworkError::Io)?);
        if include_error {
            command.stderr(output.try_clone().map_err(|_| NetworkError::Io)?);
        }
        let status = self.execute(&mut command)?;
        output.rewind().map_err(|_| NetworkError::Io)?;
        let mut text = Vec::new();
        output
            .read_to_end(&mut text)
            .map_err(|_| NetworkError::Io)?;
        let text = String::from_utf8(text).map_err(|_| NetworkError::Platform)?;
        Ok(if status.success() {
            Diagnostic::Success(text)
        } else {
            Diagnostic::Rejected(text)
        })
    }

    fn execute(&self, command: &mut Command) -> Result<ExitStatus, NetworkError> {
        let child = command.spawn().map_err(|_| NetworkError::Platform)?;
        finish_child(
            child,
            Duration::from_secs(u64::from(COMMAND_TIMEOUT_SECONDS)),
            &self.unsettled,
        )
    }
}

fn arm_child_alarm() -> std::io::Result<()> {
    // SAFETY: sigaction and the signal set are initialized before use. The
    // alarm and signal mask belong only to this process and survive exec.
    unsafe {
        let mut disposition: libc::sigaction = std::mem::zeroed();
        disposition.sa_sigaction = libc::SIG_DFL;
        let mut mask: libc::sigset_t = 0;
        if libc::sigemptyset(&raw mut disposition.sa_mask) == -1
            || libc::sigaction(libc::SIGALRM, &raw const disposition, std::ptr::null_mut()) == -1
            || libc::sigemptyset(&raw mut mask) == -1
            || libc::sigaddset(&raw mut mask, libc::SIGALRM) == -1
            || libc::sigprocmask(libc::SIG_UNBLOCK, &raw const mask, std::ptr::null_mut()) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
        libc::alarm(COMMAND_TIMEOUT_SECONDS);
    }
    Ok(())
}

fn write_capture_pid() -> std::io::Result<()> {
    let mut line = *b"Capture PID: 0000000000\n";
    // SAFETY: getpid is async-signal-safe and has no preconditions.
    let mut pid = unsafe { libc::getpid() }.unsigned_abs();
    for digit in line[CAPTURE_PID.len()..CAPTURE_PID.len() + CAPTURE_PID_DIGITS]
        .iter_mut()
        .rev()
    {
        *digit += (pid % 10) as u8;
        pid /= 10;
    }
    // SAFETY: before exec, STDOUT already points at the capture journal.
    // The initialized buffer stays live throughout this async-signal-safe call.
    let written = unsafe { libc::write(libc::STDOUT_FILENO, line.as_ptr().cast(), line.len()) };
    if usize::try_from(written) != Ok(line.len()) {
        return Err(std::io::Error::from_raw_os_error(libc::EIO));
    }
    Ok(())
}

fn finish_child(
    mut child: Child,
    timeout: Duration,
    unsettled: &Cell<bool>,
) -> Result<ExitStatus, NetworkError> {
    let pid = libc::pid_t::try_from(child.id()).map_err(|_| NetworkError::Io)?;
    let exited = match poll_until(timeout, || observe_exit(child.id())) {
        Ok(exited) => exited,
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
            unsettled.set(true);
            return Err(NetworkError::Io);
        }
        Err(_) => None,
    };

    // Even when SIGALRM exited the leader, terminate its group before
    // reaping it. waitid(WNOWAIT) kept its PID reserved for this signal.
    // SAFETY: the nonzero child PID names its private process group.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 && exited.is_none() {
        let _ = child.kill();
    }
    match poll_until(REAP_TIMEOUT, || child.try_wait()) {
        Ok(Some(status)) if exited.is_some() && status.signal() != Some(libc::SIGALRM) => {
            Ok(status)
        }
        Ok(Some(_)) => Err(NetworkError::Io),
        _ => {
            // A command stuck in the kernel can retain its descriptor until
            // it exits. Block rollback in this context as well as cross-process
            // recovery until a new context can actually acquire the lock.
            unsettled.set(true);
            let _ = std::thread::Builder::new().spawn(move || child.wait());
            Err(NetworkError::Io)
        }
    }
}

fn observe_exit(pid: u32) -> std::io::Result<Option<()>> {
    // SAFETY: the siginfo buffer is valid. With WNOHANG a zero PID means
    // there was no event. WNOWAIT observes without releasing the child's PID.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            pid,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok((info.si_pid() != 0).then_some(()))
    }
}

pub(super) fn poll_until<T>(
    timeout: Duration,
    mut observe: impl FnMut() -> std::io::Result<Option<T>>,
) -> std::io::Result<Option<T>> {
    let deadline = Instant::now() + timeout;
    loop {
        match observe() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(remaining.min(CHECK_INTERVAL));
    }
}

fn command_result(status: ExitStatus) -> Result<(), NetworkError> {
    if status.success() {
        Ok(())
    } else {
        Err(NetworkError::Platform)
    }
}

impl Commands for SystemCommands<'_> {
    fn run(&self, program: &str, args: &[&str]) -> Result<(), NetworkError> {
        command_result(self.execute(&mut self.command(program, args)?)?)
    }

    fn output(&self, program: &str, args: &[&str]) -> Result<String, NetworkError> {
        self.read_output(program, args, false)?.checked()
    }

    fn diagnostic(&self, program: &str, args: &[&str]) -> Result<Diagnostic, NetworkError> {
        self.read_output(program, args, true)
    }

    fn capture(&self, program: &str, args: &[&str], path: &str) -> Result<(), NetworkError> {
        let mut command = self.command(program, args)?;
        let journal = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| NetworkError::Io)?;
        command
            .stdout(journal.try_clone().map_err(|_| NetworkError::Io)?)
            .stderr(journal.try_clone().map_err(|_| NetworkError::Io)?);
        // SAFETY: the hook only formats a PID in a stack buffer and calls
        // async-signal-safe functions. A failed write prevents exec.
        unsafe { command.pre_exec(write_capture_pid) };
        let result = self.execute(&mut command).and_then(command_result);
        result.and(journal.sync_all().map_err(|_| NetworkError::Io))
    }

    fn read(&self, path: &str) -> Result<Option<String>, NetworkError> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(NetworkError::Io),
        }
    }

    fn modified_seconds(&self, path: &str) -> Result<u64, NetworkError> {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .map_err(|_| NetworkError::Io)?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| NetworkError::Io)
    }

    fn write(&self, path: &str, contents: &str) -> Result<(), NetworkError> {
        self.write_atomic(path, contents, true)
    }

    fn write_new(&self, path: &str, contents: &str) -> Result<(), NetworkError> {
        self.write_atomic(path, contents, false)
    }

    fn publish(&self, staging: &str, path: &str) -> Result<(), NetworkError> {
        self.ensure_settled()?;
        fs::rename(staging, path).map_err(|_| NetworkError::Io)
    }

    fn remove(&self, path: &str) -> Result<(), NetworkError> {
        self.ensure_settled()?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(NetworkError::Io),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::state::lock_exclusive;

    #[test]
    fn a_timed_out_child_releases_its_inherited_lock_before_recovery_retries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("egress.lock");
        let owner = File::create(&path).unwrap();
        let recovery = File::open(&path).unwrap();
        lock_exclusive(&owner, Duration::ZERO).unwrap();
        let mut command = SystemCommands::new(Some(&owner))
            .command("/bin/sh", &["-c", "while :; do :; done"])
            .unwrap();
        let child = command.spawn().unwrap();
        // Only the child now owns the original description, as after a crash.
        drop(command);
        drop(owner);
        assert_eq!(
            lock_exclusive(&recovery, Duration::ZERO),
            Err(NetworkError::Io)
        );
        assert!(matches!(
            finish_child(child, Duration::ZERO, &Cell::new(false)),
            Err(NetworkError::Io)
        ));
        lock_exclusive(&recovery, Duration::from_secs(1)).unwrap();
        assert_eq!(
            SystemCommands::new(Some(&recovery)).output("/usr/bin/printf", &["recovered"]),
            Ok("recovered".to_owned())
        );
    }

    #[test]
    fn output_and_journal_keep_the_tool_results() {
        let directory = tempfile::tempdir().unwrap();
        let journal = directory.path().join("pf.enable");
        let path = journal.to_str().unwrap();
        let commands = SystemCommands::new(None);
        assert_eq!(
            commands.output("/usr/bin/printf", &["ready"]),
            Ok("ready".to_owned())
        );
        assert_eq!(
            commands.run("/usr/bin/false", &[]),
            Err(NetworkError::Platform)
        );
        assert_eq!(
            commands.capture("/bin/sh", &["-c", "printf diagnostic >&2; exit 2"], path),
            Err(NetworkError::Platform)
        );
        let contents = fs::read_to_string(path).unwrap();
        let (header, output) = contents.split_once('\n').unwrap();
        assert!(
            header
                .strip_prefix(CAPTURE_PID)
                .unwrap()
                .parse::<u32>()
                .unwrap()
                > 1
        );
        assert_eq!(output, "diagnostic");
    }

    #[test]
    fn a_command_deadline_does_not_discard_the_recovery_journal() {
        let directory = tempfile::tempdir().unwrap();
        let journal = directory.path().join("pf.enable");
        assert_eq!(
            SystemCommands::new(None).capture(
                "/bin/sh",
                &["-c", "printf 'Token : 42\\n' >&2; kill -ALRM $$"],
                journal.to_str().unwrap(),
            ),
            Err(NetworkError::Io),
        );
        let contents = fs::read_to_string(journal).unwrap();
        assert!(contents.starts_with(CAPTURE_PID));
        assert!(contents.ends_with("\nToken : 42\n"));
    }

    #[test]
    fn a_context_with_a_live_timed_out_child_cannot_start_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("rules");
        let path = file.to_str().unwrap();
        let commands = SystemCommands::new(None);
        commands.write(path, "current").unwrap();
        commands.unsettled.set(true);
        assert_eq!(commands.run("/usr/bin/true", &[]), Err(NetworkError::Io));
        assert_eq!(commands.write(path, "rollback"), Err(NetworkError::Io));
        assert_eq!(commands.remove(path), Err(NetworkError::Io));
        assert_eq!(fs::read_to_string(file).unwrap(), "current");
    }

    #[test]
    fn an_alarmed_leader_cannot_leave_a_descendant_holding_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("egress.lock");
        let owner = File::create(&path).unwrap();
        let recovery = File::open(&path).unwrap();
        lock_exclusive(&owner, Duration::ZERO).unwrap();
        let mut command = SystemCommands::new(Some(&owner))
            .command(
                "/bin/sh",
                &["-c", "exec 3<&0; (while :; do :; done) & kill -ALRM $$"],
            )
            .unwrap();
        let child = command.spawn().unwrap();
        drop(command);
        drop(owner);
        assert!(
            poll_until(Duration::from_secs(1), || observe_exit(child.id()))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            lock_exclusive(&recovery, Duration::ZERO),
            Err(NetworkError::Io)
        );
        assert!(matches!(
            finish_child(child, Duration::from_secs(5), &Cell::new(false)),
            Err(NetworkError::Io)
        ));
        lock_exclusive(&recovery, Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn the_child_alarm_survives_exec() {
        let binary = std::env::current_exe().unwrap();
        let commands = SystemCommands::new(None);
        let mut command = commands
            .command(
                binary.to_str().unwrap(),
                &["--exact", "egress::commands::tests::check_child_alarm"],
            )
            .unwrap();
        command.env("CVBBMSO_TEST_CHECK_CHILD_ALARM", "1");
        command_result(commands.execute(&mut command).unwrap()).unwrap();
    }

    #[test]
    fn check_child_alarm() {
        if std::env::var_os("CVBBMSO_TEST_CHECK_CHILD_ALARM").is_some() {
            // SAFETY: this cancels the test process's own alarm and reports its remaining time.
            assert!(unsafe { libc::alarm(0) } > 0);
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod testing {
    //! Recording implementation of [`Commands`] used by the adapter tests.

    use super::{CAPTURE_PID, Commands, Diagnostic};
    use crate::NetworkError;
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    /// One command invocation.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Call {
        /// A fire-and-forget command.
        Run(String, Vec<String>),
        /// A command whose output is captured.
        Output(String, Vec<String>),
    }

    type Replies = HashMap<Vec<String>, VecDeque<Result<Diagnostic, NetworkError>>>;

    /// Records calls and serves canned file contents.
    #[derive(Debug, Default)]
    pub(crate) struct Recorder {
        pub(crate) calls: RefCell<Vec<Call>>,
        pub(crate) files: RefCell<HashMap<String, String>>,
        replies: RefCell<Replies>,
        times: RefCell<VecDeque<u64>>,
    }

    impl Recorder {
        pub(crate) fn with_file(path: &str, contents: &str) -> Self {
            let recorder = Self::default();
            recorder
                .files
                .borrow_mut()
                .insert(path.to_owned(), contents.to_owned());
            recorder
        }

        pub(crate) fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        pub(crate) fn contents(&self, path: &str) -> Option<String> {
            self.files.borrow().get(path).cloned()
        }

        pub(crate) fn times(&self, times: impl IntoIterator<Item = u64>) {
            self.times.borrow_mut().extend(times);
        }

        pub(crate) fn reply(&self, args: &[&str], result: Result<&str, NetworkError>) {
            self.record_reply(
                args,
                result.map(|text| Diagnostic::Success(text.to_owned())),
            );
        }

        pub(crate) fn rejected(&self, args: &[&str], diagnostic: &str) {
            self.record_reply(args, Ok(Diagnostic::Rejected(diagnostic.to_owned())));
        }

        fn record_reply(&self, args: &[&str], result: Result<Diagnostic, NetworkError>) {
            self.replies
                .borrow_mut()
                .entry(args.iter().map(|arg| (*arg).to_owned()).collect())
                .or_default()
                .push_back(result);
        }

        fn response(&self, args: &[&str]) -> Result<Diagnostic, NetworkError> {
            let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
            self.replies
                .borrow_mut()
                .get_mut(&args)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Ok(Diagnostic::Success(String::new())))
        }
    }

    impl Commands for Recorder {
        fn unix_seconds(&self) -> Result<u64, NetworkError> {
            Ok(self.times.borrow_mut().pop_front().unwrap_or(1_800_000_000))
        }

        fn run(&self, program: &str, args: &[&str]) -> Result<(), NetworkError> {
            self.calls.borrow_mut().push(Call::Run(
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            self.response(args)?.checked().map(drop)
        }

        fn output(&self, program: &str, args: &[&str]) -> Result<String, NetworkError> {
            self.diagnostic(program, args)?.checked()
        }

        fn diagnostic(&self, program: &str, args: &[&str]) -> Result<Diagnostic, NetworkError> {
            self.calls.borrow_mut().push(Call::Output(
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            self.response(args)
        }

        fn capture(&self, program: &str, args: &[&str], path: &str) -> Result<(), NetworkError> {
            let output = self.output(program, args)?;
            self.write(path, &format!("{CAPTURE_PID}0000000202\n{output}"))
        }

        fn read(&self, path: &str) -> Result<Option<String>, NetworkError> {
            Ok(self.files.borrow().get(path).cloned())
        }

        fn modified_seconds(&self, path: &str) -> Result<u64, NetworkError> {
            self.contents(path).ok_or(NetworkError::Io)?;
            self.unix_seconds()
        }

        fn write(&self, path: &str, contents: &str) -> Result<(), NetworkError> {
            self.files
                .borrow_mut()
                .insert(path.to_owned(), contents.to_owned());
            Ok(())
        }

        fn write_new(&self, path: &str, contents: &str) -> Result<(), NetworkError> {
            if self.files.borrow().contains_key(path) {
                return Err(NetworkError::Io);
            }
            self.write(path, contents)
        }

        fn publish(&self, staging: &str, path: &str) -> Result<(), NetworkError> {
            let mut files = self.files.borrow_mut();
            let contents = files.remove(staging).ok_or(NetworkError::Io)?;
            files.insert(path.to_owned(), contents);
            Ok(())
        }

        fn remove(&self, path: &str) -> Result<(), NetworkError> {
            self.files.borrow_mut().remove(path);
            Ok(())
        }
    }
}
