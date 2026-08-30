use derive_setters::Setters;
use rustic_core::{Metadata, Node, NodeType, ReadHandle, ReadSource};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::io::{self, Read, Seek, SeekFrom, Stdin, stdin};
use std::iter::once;
use std::path::{Path, PathBuf};

#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
/// A source to read from console input.
pub struct StdinSource {
    /// The output [`Path`] to save to.
    pub output: Option<PathBuf>,
}

impl StdinSource {
    /// Creates a new [`StdinSource`] with the path to output to.
    pub fn new(output: impl AsRef<Path>) -> Self {
        Self {
            output: Some(output.as_ref().to_path_buf()),
        }
    }

    /// Returns the configured output path, or an [`io::Error`] if none was set.
    fn get_output(&self) -> io::Result<&Path> {
        self.output.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "output path not configured")
        })
    }
}

impl ReadSource for StdinSource {
    fn location(&self) -> String {
        self.output
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    /// Opens `path` for reading.
    ///
    /// Since this source only ever exposes a single virtual file (the
    /// configured `output` path), any other path is reported as not found.
    /// Calling this more than once for the same source will hand out stdin
    /// again, which is only meaningful the first time it's actually read.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadHandle>> {
        let output = self.get_output()?;
        if path != output {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path:?} not found in stdin source"),
            ));
        }
        Ok(Box::new(StdinHandle::new()))
    }

    /// Lists the single virtual entry backed by stdin.
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
    /// Size/mtime/etc. are left at their defaults since stdin has no
    /// meaningful stat info to report ahead of reading it.
    fn stat(&self, path: &Path) -> io::Result<Option<Metadata>> {
        let output = self.get_output()?;
        Ok((path == output).then(Metadata::default))
    }
}

/// A [`ReadHandle`] that reads from process stdin.
///
/// `Stdin` is not seekable, so [`Seek::seek`] only accepts requests that
/// resolve to the current read position (i.e. a no-op rewind/tell); any other
/// seek returns [`io::ErrorKind::Unsupported`].
#[derive(Debug)]
pub struct StdinHandle {
    stdin: Stdin,
    read: u64,
}

impl StdinHandle {
    fn new() -> Self {
        Self {
            stdin: stdin(),
            read: 0,
        }
    }
}

impl Read for StdinHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.stdin.lock().read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

impl Seek for StdinHandle {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Start(0) => Ok(0),
            SeekFrom::Current(0) => Ok(self.read),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdin source does not support seeking",
            )),
        }
    }
}

impl ReadHandle for StdinHandle {
    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}
