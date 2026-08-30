use derive_setters::Setters;
use rustic_core::{CommandInput, Metadata, Node, NodeType, ReadHandle, ReadSource};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};
use std::iter::once;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

/// A source which backups a [`Command`]'s output.
#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct CommandSource {
    /// The output [`Path`] to save to.
    pub output: Option<PathBuf>,

    /// The [`CommandInput`] to use.
    pub command: Option<CommandInput>,
}

impl CommandSource {
    /// Creates a new [`CommandSource`] with the given command.
    pub fn new(cmd: &CommandInput, output: impl AsRef<Path>) -> Self {
        Self {
            output: Some(output.as_ref().to_path_buf()),
            command: Some(cmd.to_owned()),
        }
    }

    /// Returns the configured output path, or an [`io::Error`] if none was set.
    fn get_output(&self) -> io::Result<&Path> {
        self.output.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "output path not configured")
        })
    }

    /// Returns the configured command, or an [`io::Error`] if none was set.
    fn get_command(&self) -> io::Result<&CommandInput> {
        self.command
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "command not configured"))
    }
}

impl ReadSource for CommandSource {
    fn location(&self) -> String {
        self.output
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    /// Spawns the configured command and hands back its stdout as a
    /// [`ReadHandle`].
    ///
    /// The child is spawned fresh on every call — this mirrors `open_read`
    /// being the natural "open this file" hook, at the cost of re-running the
    /// command if `open_read` is ever called more than once for the same
    /// source. If your call site only ever opens each source once, that's a
    /// non-issue.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadHandle>> {
        let output = self.get_output()?;
        if path != output {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path:?} not found in command source"),
            ));
        }

        let cmd = self.get_command()?;
        let child = Command::new(cmd.command())
            .args(cmd.args())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|err| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("failed to spawn `{}`: {err}", cmd.command()),
                )
            })?;

        Ok(Box::new(CommandHandle::new(child)?))
    }

    /// Lists the single virtual entry backed by the command's stdout.
    ///
    /// Only the root (`""` or `/`) yields anything; this source has no real
    /// directory structure, just one file.
    fn readdir(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<Node>> + Send>> {
        let output = self.get_output()?;
        if path != Path::new("") && path != Path::new("/") {
            return Ok(Box::new(std::iter::empty()));
        }

        let name = output.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "output path has no file name")
        })?;

        let node = Node::new_node(name, NodeType::File, Metadata::default());
        Ok(Box::new(once(Ok(node))))
    }

    /// Returns metadata for `path` if it matches the configured output path.
    ///
    /// Size/mtime/etc. are left at their defaults since the command's output
    /// size isn't known ahead of running it.
    fn stat(&self, path: &Path) -> io::Result<Option<Metadata>> {
        let output = self.get_output()?;
        Ok((path == output).then(Metadata::default))
    }
}

/// A [`ReadHandle`] that reads a spawned child process's stdout.
///
/// [`ReadHandle::close`] waits for the child to exit and surfaces a
/// non-zero exit status as an error, folding in the old `finish` /
/// `handle_status` behavior from `StdoutReader::finish`.
pub struct CommandHandle {
    child: Child,
    stdout: ChildStdout,
    read: u64,
}

impl CommandHandle {
    fn new(mut child: Child) -> io::Result<Self> {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "child process has no stdout"))?;
        Ok(Self {
            child,
            stdout,
            read: 0,
        })
    }
}

impl fmt::Debug for CommandHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandHandle")
            .field("pid", &self.child.id())
            .field("read", &self.read)
            .finish()
    }
}

impl Read for CommandHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stdout.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

impl Seek for CommandHandle {
    /// `ChildStdout` is not seekable, so only requests that resolve to the
    /// current position (rewind-to-zero-if-nothing-read, or a "tell") are
    /// accepted; anything else errors.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Start(0) => Ok(0),
            SeekFrom::Current(0) => Ok(self.read),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "command source does not support seeking",
            )),
        }
    }
}

impl ReadHandle for CommandHandle {
    fn close(&mut self) -> io::Result<()> {
        let status = self.child.wait()?;
        if !status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("command exited with {status}"),
            ));
        }
        Ok(())
    }
}
