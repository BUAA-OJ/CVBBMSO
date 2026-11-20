//! Commands and files the platform adapters need.
//!
//! The trait exists as a test seam: the real implementation shells out to the
//! platform tools, tests record the calls instead of touching the firewall.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

use crate::NetworkError;

/// Runs platform tools and reads or writes the files they need.
#[cfg_attr(
    not(any(target_os = "macos", windows)),
    allow(dead_code, reason = "only the platform adapters call these methods")
)]
pub(crate) trait Commands {
    /// Runs `program` and fails unless it exits successfully.
    fn run(&self, program: &str, args: &[&str]) -> Result<(), NetworkError>;

    /// Runs `program` and returns its standard output.
    fn output(&self, program: &str, args: &[&str]) -> Result<String, NetworkError>;

    /// Runs a command with its output written directly to a new journal.
    fn capture(&self, program: &str, args: &[&str], path: &str) -> Result<(), NetworkError>;

    /// Reads a file, or returns `None` when it does not exist.
    fn read(&self, path: &str) -> Result<Option<String>, NetworkError>;

    /// Writes a file, replacing its contents.
    fn write(&self, path: &str, contents: &str) -> Result<(), NetworkError>;

    /// Removes a file, succeeding when it does not exist.
    fn remove(&self, path: &str) -> Result<(), NetworkError>;
}

/// The inherited lock keeps recovery from overtaking an orphaned platform command.
pub(crate) struct SystemCommands<'a>(pub(crate) Option<&'a File>);

impl SystemCommands<'_> {
    fn command(&self, program: &str, args: &[&str]) -> Result<Command, NetworkError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir("/")
            .env_clear()
            .env("LC_ALL", "C");
        if let Some(lock) = self.0 {
            command.stdin(lock.try_clone().map_err(|_| NetworkError::Io)?);
        }
        Ok(command)
    }
}

impl Commands for SystemCommands<'_> {
    fn run(&self, program: &str, args: &[&str]) -> Result<(), NetworkError> {
        self.output(program, args).map(drop)
    }

    fn output(&self, program: &str, args: &[&str]) -> Result<String, NetworkError> {
        let output = self
            .command(program, args)?
            .output()
            .map_err(|_| NetworkError::Platform)?;
        if output.status.success() {
            String::from_utf8(output.stdout).map_err(|_| NetworkError::Platform)
        } else {
            Err(NetworkError::Platform)
        }
    }

    fn capture(&self, program: &str, args: &[&str], path: &str) -> Result<(), NetworkError> {
        let journal = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| NetworkError::Io)?;
        let stdout = journal.try_clone().map_err(|_| NetworkError::Io)?;
        let status = self
            .command(program, args)?
            .stdout(stdout)
            .stderr(journal)
            .status()
            .map_err(|_| NetworkError::Platform)?;
        if status.success() {
            Ok(())
        } else {
            Err(NetworkError::Platform)
        }
    }

    fn read(&self, path: &str) -> Result<Option<String>, NetworkError> {
        match fs::read_to_string(path) {
            Ok(contents) => Ok(Some(contents)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(NetworkError::Io),
        }
    }

    fn write(&self, path: &str, contents: &str) -> Result<(), NetworkError> {
        let path = Path::new(path);
        let directory = path.parent().ok_or(NetworkError::Io)?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).map_err(|_| NetworkError::Io)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => {
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
        temporary.persist(path).map_err(|_| NetworkError::Io)?;
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<(), NetworkError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(NetworkError::Io),
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) mod testing {
    //! Recording implementation of [`Commands`] used by the adapter tests.

    use super::Commands;
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

    type Replies = HashMap<Vec<String>, VecDeque<Result<String, NetworkError>>>;

    /// Records calls and serves canned file contents.
    #[derive(Debug, Default)]
    pub(crate) struct Recorder {
        pub(crate) calls: RefCell<Vec<Call>>,
        pub(crate) files: RefCell<HashMap<String, String>>,
        replies: RefCell<Replies>,
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

        pub(crate) fn reply(&self, args: &[&str], result: Result<&str, NetworkError>) {
            self.replies
                .borrow_mut()
                .entry(args.iter().map(|arg| (*arg).to_owned()).collect())
                .or_default()
                .push_back(result.map(str::to_owned));
        }

        fn response(&self, args: &[&str]) -> Result<String, NetworkError> {
            let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
            self.replies
                .borrow_mut()
                .get_mut(&args)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Ok(String::new()))
        }
    }

    impl Commands for Recorder {
        fn run(&self, program: &str, args: &[&str]) -> Result<(), NetworkError> {
            self.calls.borrow_mut().push(Call::Run(
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            self.response(args).map(drop)
        }

        fn output(&self, program: &str, args: &[&str]) -> Result<String, NetworkError> {
            self.calls.borrow_mut().push(Call::Output(
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            self.response(args)
        }

        fn capture(&self, program: &str, args: &[&str], path: &str) -> Result<(), NetworkError> {
            let output = self.output(program, args)?;
            self.write(path, &output)
        }

        fn read(&self, path: &str) -> Result<Option<String>, NetworkError> {
            Ok(self.files.borrow().get(path).cloned())
        }

        fn write(&self, path: &str, contents: &str) -> Result<(), NetworkError> {
            self.files
                .borrow_mut()
                .insert(path.to_owned(), contents.to_owned());
            Ok(())
        }

        fn remove(&self, path: &str) -> Result<(), NetworkError> {
            self.files.borrow_mut().remove(path);
            Ok(())
        }
    }
}
