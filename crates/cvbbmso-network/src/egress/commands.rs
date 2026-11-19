//! Commands and files the platform adapters need.
//!
//! The trait exists as a test seam: the real implementation shells out to the
//! platform tools, tests record the calls instead of touching the firewall.

use std::fs;
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

    /// Reads a file, or returns `None` when it does not exist.
    fn read(&self, path: &str) -> Result<Option<String>, NetworkError>;

    /// Writes a file, replacing its contents.
    fn write(&self, path: &str, contents: &str) -> Result<(), NetworkError>;

    /// Removes a file, succeeding when it does not exist.
    fn remove(&self, path: &str) -> Result<(), NetworkError>;
}

/// Commands backed by the operating system.
pub(crate) struct SystemCommands;

impl Commands for SystemCommands {
    fn run(&self, program: &str, args: &[&str]) -> Result<(), NetworkError> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|_| NetworkError::Platform)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(NetworkError::Platform)
        }
    }

    fn output(&self, program: &str, args: &[&str]) -> Result<String, NetworkError> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|_| NetworkError::Platform)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
        if let Some(parent) = std::path::Path::new(path).parent() {
            fs::create_dir_all(parent).map_err(|_| NetworkError::Io)?;
        }
        fs::write(path, contents).map_err(|_| NetworkError::Io)
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

    /// One command invocation.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Call {
        /// A fire-and-forget command.
        Run(String, Vec<String>),
        /// A command whose output is captured.
        Output(String, Vec<String>),
    }

    /// Records calls and serves canned file contents.
    #[derive(Debug, Default)]
    pub(crate) struct Recorder {
        pub(crate) calls: RefCell<Vec<Call>>,
        pub(crate) files: RefCell<std::collections::HashMap<String, String>>,
        pub(crate) fail_on: RefCell<Option<String>>,
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

        fn fail_if(&self, program: &str) -> Result<(), NetworkError> {
            if self.fail_on.borrow().as_deref() == Some(program) {
                return Err(NetworkError::Platform);
            }
            Ok(())
        }
    }

    impl Commands for Recorder {
        fn run(&self, program: &str, args: &[&str]) -> Result<(), NetworkError> {
            self.calls.borrow_mut().push(Call::Run(
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            self.fail_if(program)
        }

        fn output(&self, program: &str, args: &[&str]) -> Result<String, NetworkError> {
            self.calls.borrow_mut().push(Call::Output(
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            self.fail_if(program)?;
            Ok(String::new())
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
