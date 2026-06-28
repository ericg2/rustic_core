use bytes::Bytes;
use log::{error, trace};
use opendal::{
    Builder,
    blocking::Operator,
    layers::{ConcurrentLimitLayer, LoggingLayer, RetryLayer, ThrottleLayer},
    options::ReadOptions,
};
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use std::path::Path;
use std::sync::OnceLock;
use tokio::runtime::Runtime;
use typed_path::UnixPathBuf;

use crate::opendal::OpenDALSource;
use crate::opendal::config::*;
use crate::opendal::log::OpenLogLayer;
use crate::opendal::source::OpenDALReader;
use rustic_core::{
    ALL_FILE_TYPES, ErrorKind, FileType, Id, ReadBackend, ReadSource, ReadSourceBuilder,
    RusticError, RusticResult, WriteBackend,
};

mod constants {
    /// Default number of retries
    pub(super) const DEFAULT_RETRY: u32 = 5;

    /// Default number of connections.
    pub(super) const DEFAULT_CONNECTIONS: u32 = 8;
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

/// `OpenDALPath` contains a wrapper around a blocking operator of the `OpenDAL` library.
#[derive(Clone, Debug)]
pub struct OpenDALBackend {
    pub(crate) operator: Operator,
    pub(crate) config: OpenDALConfig,
}

impl OpenDALBackend {
    pub(crate) fn new(config: &OpenDALConfig) -> RusticResult<Self> {
        let mut operator = config.scheme().operator().map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Creating Operator from path failed. Please check the given schema and options.",
                err,
            )
        })?;

        // TODO: add the extra retry options using ExponentialBackoff.
        let retry = config.retry.get_setting(constants::DEFAULT_RETRY as usize);
        operator = operator.layer(RetryLayer::new().with_max_times(retry).with_jitter());
        operator = operator.layer(LoggingLayer::new(OpenLogLayer));

        if let Some(x) = config.connections {
            operator = operator.layer(ConcurrentLimitLayer::new(x as usize));
        }

        if let Some(ref x) = config.throttle {
            operator = operator.layer(ThrottleLayer::new(x.bandwidth, x.burst));
        }

        let op = operator.layer(LoggingLayer::new(OpenLogLayer));
        let operator = if tokio::runtime::Handle::try_current().is_ok() {
            // Async context: block_in_place yields the thread to Tokio safely
            tokio::task::block_in_place(|| Operator::new(op))
        } else {
            // Sync context: no runtime at all, just call it directly
            Operator::new(op)
        }
        .map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Creating blocking Operator from path failed.",
                err,
            )
        })?;

        Ok(Self {
            operator,
            config: config.to_owned(),
        })
    }

    pub(crate) fn get_reader(&self, path: impl AsRef<Path>) -> RusticResult<OpenDALReader> {
        let src = OpenDALSource::new(&self.config, path.as_ref());
        src.get_reader()
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

impl ReadBackend for OpenDALBackend {
    /// Returns the location of the backend.
    ///
    /// This is `opendal:<scheme>:<name>` (e.g., `opendal:gdrive:` for Google Drive).
    fn location(&self) -> String {
        let info = self.operator.info();
        format!("opendal:{}:{}", info.scheme(), info.name())
    }

    /// Lists all files with their size of the given type.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the files to list.
    ///
    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        fn length(size: u64, file_name: &str, tpe: FileType) -> Option<u32> {
            size.try_into().inspect_err(|err| {
                error!("Failed to convert file length {size} of {file_name} to u32 while listing {tpe}: {err}");
            }).ok()
        }

        trace!("listing tpe: {tpe:?}");
        if tpe == FileType::Config {
            return match self.operator.stat("config") {
                Ok(meta) => Ok(vec![(Id::default(), length(meta.content_length(), "config", tpe).unwrap_or_default())]),
                Err(err) if err.kind() == opendal::ErrorKind::NotFound => Ok(Vec::new()),
                Err(err) => Err(err).map_err(|err|
                    RusticError::with_source(
                        ErrorKind::Backend,
                        "Getting Metadata of type `{type}` failed in the backend. Please check if `{type}` exists.",
                        err,
                    )
                        .attach_context("type", tpe.to_string())
                ),
            };
        }

        let path = tpe.dirname().to_string() + "/";
        let entries = self
            .get_reader(Path::new(&path))?
            .entries()
            .filter_map(Result::ok) // errors already caught
            .filter_map(|entry| {
                if entry.node.is_file() {
                    let name = &entry.node.name;
                    let id = Id::parse_some(name, tpe)?;
                    let length = length(entry.node.meta.size, name, tpe)?;
                    Some((id, length))
                } else {
                    None
                }
            })
            .collect();

        Ok(entries)
    }

    /// Lists all files of the given type.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the files to list.
    ///
    /// # Notes
    ///
    /// If the file type is `FileType::Config`, this will return a list with a single default id.
    fn list(&self, tpe: FileType) -> RusticResult<Vec<Id>> {
        trace!("listing tpe: {tpe:?}");
        if tpe == FileType::Config {
            return Ok(
                if self.operator.exists("config").map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::Backend,
                        "Path `config` does not exist.",
                        err,
                    )
                    .ask_report()
                })? {
                    vec![Id::default()]
                } else {
                    Vec::new()
                },
            );
        }

        let path = tpe.dirname().to_string() + "/";
        let entries = self
            .get_reader(Path::new(&path))?
            .entries()
            .filter_map(Result::ok) // errors already caught
            .filter_map(|entry| {
                if entry.node.is_file() {
                    Some(Id::parse_some(&entry.node.name, tpe)?)
                } else {
                    None
                }
            })
            .collect();

        Ok(entries)
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        trace!("reading tpe: {tpe:?}, id: {id}");

        let path = self.path(tpe, id);
        Ok(self
            .operator
            .read(&path)
            .map_err(|err|
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Reading file `{path}` failed in the backend. Please check if the given path is correct.",
                    err,
                )
                    .attach_context("path", path)
                    .attach_context("type", tpe.to_string())
                    .attach_context("id", id.to_string())
            )?
            .to_bytes())
    }

    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        _cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes> {
        trace!("reading tpe: {tpe:?}, id: {id}, offset: {offset}, length: {length}");
        let range = u64::from(offset)..u64::from(offset + length);
        let path = self.path(tpe, id);
        let read_options = ReadOptions {
            range: range.into(),
            ..Default::default()
        };

        Ok(self
            .operator
            .read_options(&path, read_options)
            .map_err(|err|
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Partially reading file `{path}` failed in the backend. Please check if the given path is correct.",
                    err,
                )
                    .attach_context("path", path)
                    .attach_context("type", tpe.to_string())
                    .attach_context("id", id.to_string())
                    .attach_context("offset", offset.to_string())
                    .attach_context("length", length.to_string())
            )?
            .to_bytes())
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        // OpenDAL normalizes roots to format `/path/` (with leading and trailing slashes)
        // or just `/` for the storage root. We strip these slashes to get the root prefix
        // and prepend it to the relative path for the warm-up command.
        // This ensures warm-up commands receive the full S3 object key like
        // `rustic/data/03/03dc1178...` instead of just `data/03/03dc1178...`
        let root = self.operator.info().root();
        let root = root.trim_matches('/');
        let relative_path = self.path(tpe, id);
        if root.is_empty() {
            relative_path
        } else {
            format!("{root}/{relative_path}")
        }
    }
}

impl WriteBackend for OpenDALBackend {
    /// Create a repository on the backend.
    fn create(&self) -> RusticResult<()> {
        trace!("creating repo at {:?}", self.location());

        for tpe in ALL_FILE_TYPES {
            let path = tpe.dirname().to_string() + "/";
            self.operator
                .create_dir(&path)
                .map_err(|err|
                    RusticError::with_source(
                        ErrorKind::Backend,
                        "Creating directory `{path}` failed in the backend `{location}`. Please check if the given path is correct.",
                        err,
                    )
                        .attach_context("path", path)
                        .attach_context("location", self.location())
                        .attach_context("type", tpe.to_string())
                )?;
        }
        // creating 256 dirs can be slow on remote backends, hence we parallelize it.
        (0u8..=255)
            .into_par_iter()
            .try_for_each(|i| {
                let path = UnixPathBuf::from("data")
                    .join(hex::encode([i]))
                    .to_string_lossy()
                    .to_string()
                    + "/";

                self.operator.create_dir(&path).map_err(|err|
                    RusticError::with_source(
                        ErrorKind::Backend,
                        "Creating directory `{path}` failed in the backend `{location}`. Please check if the given path is correct.",
                        err,
                    )
                        .attach_context("path", path)
                        .attach_context("location", self.location())
                )
            })?;

        Ok(())
    }

    /// Write the given bytes to the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `cacheable` - Whether the file is cacheable.
    /// * `buf` - The bytes to write.
    fn write_bytes(
        &self,
        tpe: FileType,
        id: &Id,
        _cacheable: bool,
        buf: Bytes,
    ) -> RusticResult<()> {
        trace!("writing tpe: {:?}, id: {}", &tpe, &id);
        let filename = self.path(tpe, id);
        _ = self.operator.write(&filename, buf).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Writing file `{path}` failed in the backend. Please check if the given path is correct.",
                err,
            )
                .attach_context("path", filename)
                .attach_context("type", tpe.to_string())
                .attach_context("id", id.to_string())
        })?;

        Ok(())
    }

    /// Remove the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `cacheable` - Whether the file is cacheable.
    fn remove(&self, tpe: FileType, id: &Id, _cacheable: bool) -> RusticResult<()> {
        trace!("removing tpe: {:?}, id: {}", &tpe, &id);
        let filename = self.path(tpe, id);
        self.operator.delete(&filename).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Deleting file `{path}` failed in the backend. Please check if the given path is correct.",
                err,
            )
                .attach_context("path", filename)
                .attach_context("type", tpe.to_string())
                .attach_context("id", id.to_string())
        })?;
        Ok(())
    }
}
