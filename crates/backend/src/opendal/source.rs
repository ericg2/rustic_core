use bytes::Bytes;
use opendal::Buffer;
use opendal::blocking::{Operator, StdReader, StdWriter};
use opendal::layers::{ConcurrentLimitLayer, LoggingLayer, RetryLayer, ThrottleLayer};
use opendal::options::{DeleteOptions, ListOptions, WriteOptions};
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::sync::OnceLock;
use tokio::runtime::Runtime;
use typed_path::UnixPathBuf;

use crate::opendal::config::{OpenDALConfig, Retry, Throttle};
use crate::opendal::log::OpenLogLayer;
use rustic_core::{
    ErrorKind, FileLister, FileType, Id, Metadata, Node, NodeType, ReadBackend, ReadHandle,
    ReadSource, ReadSourceConfig, RusticError, RusticResult, WriteBackend, WriteHandle,
    WriteSource,
};

mod constants {
    /// Default number of retries
    pub(super) const DEFAULT_RETRY: usize = 5;

    /// Default number of connections.
    pub(super) const DEFAULT_CONNECTIONS: usize = 8;
}

fn runtime() -> tokio::runtime::Handle {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    tokio::runtime::Handle::try_current().unwrap_or_else(|_| {
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
            })
            .handle()
            .clone()
    })
}

/// `OpenDALSource` contains a wrapper around a blocking operator of the `OpenDAL` library.
#[derive(Clone, Debug)]
pub struct OpenDALSource {
    op: Operator,
}

impl OpenDALSource {
    pub fn new(op: Operator) -> Self {
        Self { op }
    }

    pub fn from_config(config: &OpenDALConfig) -> RusticResult<Self> {
        let max_retries = match config.retry {
            Some(Retry::Off) => 0,
            Some(Retry::Custom(value)) => value,
            None | Some(Retry::Default) => constants::DEFAULT_RETRY,
        };

        let connections = config.connections;
        let throttle = config.throttle;
        let scheme = config.scheme.as_deref().ok_or_else(|| {
            RusticError::new(
                ErrorKind::InvalidInput,
                "No scheme given in OpenDAL config.",
            )
        })?;

        let mut operator = opendal::Operator::via_iter(scheme, config.options.clone())
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Creating Operator from scheme `{scheme}` failed. Please check the given schema and options.",
                    err,
                )
                    .attach_context("scheme", scheme.to_string())
            })?
            .layer(RetryLayer::new().with_max_times(max_retries).with_jitter());

        if let Some(Throttle { bandwidth, burst }) = throttle {
            operator = operator.layer(ThrottleLayer::new(bandwidth, burst));
        }

        if let Some(connections) = connections {
            operator = operator.layer(ConcurrentLimitLayer::new(connections));
        }

        let _guard = runtime().enter();
        let op = Operator::new(operator.layer(LoggingLayer::new(OpenLogLayer))).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Creating blocking Operator from scheme `{scheme}` failed.",
                err,
            )
            .attach_context("scheme", scheme.to_string())
        })?;

        Ok(Self { op })
    }

    /// Converts a [`Path`] into an OpenDAL-supported [`String`].
    ///
    /// # Arguments
    /// * `base` - The root [`Path`] to use.
    /// * `p` - The [`Path`] to convert from.
    /// * `is_dir` - If representing a directory or file.
    ///
    /// # Returns
    /// A valid [`String`] for OpenDAL use.
    pub(crate) fn fix_path(p: impl AsRef<Path>, is_dir: bool) -> String {
        let mut r = p.as_ref().to_string_lossy().to_string();
        if !r.starts_with("/") {
            r = format!("/{r}")
        }
        if is_dir && !r.ends_with("/") {
            r += "/"
        } else if !is_dir && r.ends_with("/") {
            r = r.strip_suffix("/").unwrap_or(&r).to_string()
        }
        r.replace("\\", "/") // *** fix for windows-style directories
    }

    pub(crate) fn resolve_meta(meta: &opendal::Metadata) -> Metadata {
        Metadata {
            atime: meta
                .last_modified()
                .map(opendal::raw::Timestamp::into_inner),
            size: meta.content_length(),
            ..Default::default()
        }
    }

    /// Return a path for the given file type and id.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    ///
    /// # Returns
    ///
    /// The path for the given file type and id.
    // Let's keep this for now, as it's being used in the trait implementations.
    #[allow(clippy::unused_self)]
    fn path(&self, tpe: FileType, id: &Id) -> String {
        let hex_id = id.to_hex();
        match tpe {
            FileType::Config => UnixPathBuf::from("config"),
            FileType::Pack => UnixPathBuf::from("data")
                .join(&hex_id[0..2])
                .join(&hex_id[..]),
            _ => UnixPathBuf::from(tpe.dirname()).join(&hex_id[..]),
        }
        .to_string()
    }
}

struct OpenDALWrite(StdWriter);

impl WriteHandle for OpenDALWrite {
    fn close(&mut self) -> std::io::Result<()> {
        self.0.flush()?;
        self.0.close()?;
        Ok(())
    }
}

impl Write for OpenDALWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

struct OpenDALRead(StdReader);

impl ReadHandle for OpenDALRead {
    fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Read for OpenDALRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for OpenDALRead {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl ReadSource for OpenDALSource {
    fn location(&self) -> String {
        let info = self.op.info();
        format!("opendal:{}:{}", info.scheme(), info.name())
    }

    fn open_read(&self, path: &Path) -> std::io::Result<Box<dyn ReadHandle>> {
        let path = Self::fix_path(path, false);
        let handle = self.op.reader(&path)?.into_std_read(..)?;
        Ok(Box::new(OpenDALRead(handle)))
    }
    fn readdir(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<Node>> + Send>> {
        let path = Self::fix_path(path, true);

        let lister = self
            .op
            .lister_options(
                &path,
                ListOptions {
                    recursive: false,
                    ..Default::default()
                },
            )?
            .filter_map(move |entry| {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => return Some(Err(io::Error::other(err))),
                };

                let entry_path = entry.path();
                if Self::fix_path(entry_path, true) == path {
                    return None;
                }

                let meta = entry.metadata();

                let node_type = if meta.is_dir() {
                    NodeType::Dir
                } else {
                    NodeType::File
                };

                let name = entry_path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string();

                Some(Ok(Node::new(
                    name,
                    node_type,
                    Self::resolve_meta(meta),
                    None,
                    None,
                )))
            });

        Ok(Box::new(lister))
    }

    fn stat(&self, path: &Path) -> std::io::Result<Option<rustic_core::Metadata>> {
        let path = Self::fix_path(path, false);
        match self.op.stat(&path) {
            Ok(meta) => Ok(Some(Self::resolve_meta(&meta))),
            Err(err) if err.kind() == opendal::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        }
    }
}

impl WriteSource for OpenDALSource {
    fn remove_dir(&self, path: &Path) -> std::io::Result<()> {
        let path = Self::fix_path(path, true);
        self.op.delete_options(
            &path,
            DeleteOptions {
                recursive: true,
                ..Default::default()
            },
        )?;
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        let path = Self::fix_path(path, false);
        self.op.delete(&path)?;
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        let path = Self::fix_path(path, true);
        if path != "/" {
            // OpenDAL does not allow creating a root directory. Don't do this on restore!
            self.op.create_dir(&path)?;
        }
        Ok(())
    }

    fn set_restore_metadata(
        &self,
        _path: &Path,
        _node: &rustic_core::Node,
        _opts: &rustic_core::RestoreOptions,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn set_length(&self, path: &Path, size: u64) -> std::io::Result<()> {
        let path = Self::fix_path(path, false);
        if size == 0 {
            self.op.write(&path, Buffer::new())?;
            return Ok(());
        }

        // OpenDAL doesn't provide a generic truncate API.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Cannot set OpenDAL length > 0",
        ))
    }

    fn open_replace(&self, path: &Path) -> std::io::Result<Box<dyn WriteHandle>> {
        let handle = self
            .op
            .writer_options(
                &path.to_string_lossy().to_string(),
                WriteOptions {
                    append: false,
                    ..Default::default()
                },
            )
            .and_then(|r| Ok(r.into_std_write()))?;
        Ok(Box::new(OpenDALWrite(handle)))
    }

    fn write_all(&self, path: &Path, bytes: Bytes) -> std::io::Result<()> {
        let path = Self::fix_path(path, false);
        self.op.write(&path, bytes)?;
        Ok(())
    }

    fn write_at(&self, _path: &Path, offset: u64, data: &[u8]) -> std::io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn hard_link(&self, path: &Path, item: &Path) -> std::io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn can_random_write(&self) -> bool {
        false
    }

    fn can_hard_link(&self) -> bool {
        false
    }
}
