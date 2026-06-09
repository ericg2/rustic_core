use derive_setters::Setters;
use rustic_core::{
    CommandInput, CommandInputErrorKind, ErrorKind, ReadSource, ReadSourceBuilder, ReadSourceEntry,
    RusticError, RusticResult,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::iter::{once, Once};
use std::path::Path;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::Mutex;
use std::{path::PathBuf, process::Command};

/// A source which backups a [`Command`] output.
#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Debug, Setters, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct CommandSource {
    #[setters(skip)]
    output: PathBuf,

    #[setters(skip)]
    command: CommandInput,
}

impl CommandSource {
    /// Creates a new [`CommandSource`] with the given command.
    pub fn new(cmd: impl Into<CommandInput>, output: impl AsRef<Path>) -> Self {
        Self {
            output: output.as_ref().to_path_buf(),
            command: cmd.into(),
        }
    }
}

impl ReadSourceBuilder for CommandSource {
    type Reader = StdoutReader;

    fn get_reader(&self) -> RusticResult<Self::Reader> {
        StdoutReader::new(&self)
    }
}

/// The `StdoutReader` is a `ReadSource` when spawning a child process and reading its stdout
#[derive(Debug)]
pub struct StdoutReader {
    /// The path of the stdin entry.
    config: CommandSource,

    /// The child process
    ///
    /// # Note
    ///
    /// This is in a Mutex as we want to take out `ChildStdout`
    /// in the `entries` method - but this method only gets a
    /// reference of self.
    process: Mutex<Child>,
}

impl StdoutReader {
    /// Creates a new `ChildSource`.
    ///
    /// # Errors
    /// - if calling the command fails
    pub(crate) fn new(config: &CommandSource) -> RusticResult<Self> {
        let cmd = config.command.clone();
        let process = Command::new(cmd.command())
            .args(cmd.args())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|err| CommandInputErrorKind::ProcessExecutionFailed {
                command: cmd.clone(),
                path: config.output.clone(),
                source: err,
            });

        Ok(Self {
            process: Mutex::new(cmd.on_failure().display_result(process)?),
            config: config.to_owned(),
        })
    }

    /// Finishes the `ChildSource`
    ///
    /// # Errors
    /// - if handling of the return code leads to an error
    ///
    /// # Panics
    /// - if the lock for the process cannot be obtained (should not happen)
    pub fn finish(self) -> RusticResult<()> {
        let status = self.process.lock().unwrap().wait();
        self.config
            .command
            .on_failure()
            .handle_status(status, "stdin-command", "call")?;
        Ok(())
    }
}

impl ReadSource for StdoutReader {
    type Open = ChildStdout;
    type Iter = Once<RusticResult<ReadSourceEntry<ChildStdout>>>;

    fn size(&self) -> RusticResult<Option<u64>> {
        Ok(None)
    }

    fn entries(&self) -> Self::Iter {
        let open = self.process.lock().unwrap().stdout.take();
        once(
            ReadSourceEntry::from_path(self.config.output.clone(), open).map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Failed to create ReadSourceEntry from ChildStdout",
                    err,
                )
            }),
        )
    }

    fn paths(&self) -> Vec<PathBuf> {
        vec![self.config.output.clone()]
    }
}
