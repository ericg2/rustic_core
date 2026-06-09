use bytes::Bytes;
use bytesize::ByteSize;
use derive_setters::Setters;
use log::{error, trace, warn};
use opendal::raw::Access;
use opendal::{
    Builder, Configurator, Entry, IntoOperatorUri, OperatorBuilder,
    blocking::{Operator, StdReader},
    layers::{ConcurrentLimitLayer, LoggingLayer, RetryLayer, ThrottleLayer},
    options::{ListOptions, ReadOptions},
};
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use std::path::Path;
/// `OpenDAL` backend for rustic.
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    str::FromStr,
    sync::{Arc, OnceLock},
    vec::IntoIter,
};
use tokio::runtime::Runtime;
use typed_path::UnixPathBuf;

use crate::opendal::config::*;
use crate::opendal::opendal_src::OpenDALReader;
use crate::opendal::scheme::Schemeable;
use crate::opendal::{OpenDALDestination, OpenDALSource, Throttle};
use crate::retry::RetrySetting;
use rustic_core::{
    ALL_FILE_TYPES, ErrorKind, Excludes, FileType, Id, Metadata, PathList, ReadBackend,
    ReadFileOpen, ReadSource, ReadSourceBuilder, ReadSourceEntry, RepositoryConfig, RusticError,
    RusticResult, WriteBackend,
    repofile::{Node, NodeType},
};

mod constants {
    /// Default number of retries
    pub(super) const DEFAULT_RETRY: u32 = 5;

    /// Default number of connections.
    pub(super) const DEFAULT_CONNECTIONS: u32 = 8;
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    })
}

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Setters)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct OpenDALConfig {
    /// The maximum connections.
    #[serde(alias = "connections", alias = "max_connections")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    connections: Option<u32>,

    #[serde_as(as = "Option<DisplayFromStr>")]
    throttle: Option<Throttle>,

    #[serde_as(as = "DisplayFromStr")]
    retry: RetrySetting,

    /// The OpenDAL Scheme to use.
    #[setters(skip)]
    scheme: String,

    /// The serialized config.
    #[setters(skip)]
    #[serde(flatten)]
    config: HashMap<String, String>,
}

impl OpenDALConfig {
    /// Creates a new openDAL backend via dynamic types.
    pub fn from_iter<K, V, I>(scheme: impl AsRef<str>, dict: I) -> RusticResult<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map: HashMap<String, String> = dict
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();

        let scheme = scheme
            .as_ref()
            .split(':')
            .next()
            .unwrap_or_else(|| scheme.as_ref());

        // inject scheme so serde can populate it
        map.insert("scheme".to_string(), scheme.to_string());

        let config: Self = serde_json::to_value(map)
            .and_then(serde_json::from_value)
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Configuration,
                    "Failed to deserialize OpenDAL config",
                    err,
                )
            })?;

        Ok(config)
    }

    /// Creates a new openDAL backend via a [`Builder`].
    ///
    ///
    /// # Arguments
    ///
    /// * `be` - The [`Builder`] to use. Ex: [`S3`] or [`B2`]
    ///
    /// # Example
    ///
    /// ```
    /// use opendal::services::{S3Config, S3};
    /// use rustic_backend::opendal::{OpenDALBackend, OpenDALConfig};
    ///
    /// let config = S3::default()
    ///     .bucket("abcd")
    ///     .access_key_id("SECRET")
    ///     .endpoint("127.0.0.1");
    ///
    /// let be = OpenDALConfig::from_builder(config);
    /// ```
    pub fn new(be: impl Schemeable) -> Self {
        Self {
            config: be.config(),
            scheme: String::from(be.scheme()),
            retry: RetrySetting::Default,
            connections: None,
            throttle: None,
        }
    }

    pub fn restore_to(&self, path: impl AsRef<Path>) -> OpenDALDestination {
        OpenDALDestination::new(&self, path)
    }

    pub fn backup_from(&self, path: impl Into<PathList>) -> OpenDALSource {
        OpenDALSource::new(&self, path)
    }
}

impl RepositoryConfig for OpenDALConfig {
    fn get_path(&self) -> String {
        format!("opendal:{}", &self.scheme)
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self);
        ret.remove("scheme");
        ret
    }

    fn get_repo(&self) -> RusticResult<Arc<dyn WriteBackend>> {
        // OpenDALBackend is impl WriteBackend
        let ret = OpenDALBackend::new(&self)?;
        Ok(Arc::new(ret))
    }
}

/// `OpenDALPath` contains a wrapper around a blocking operator of the `OpenDAL` library.
#[derive(Clone, Debug)]
pub(crate) struct OpenDALBackend {
    pub(crate) operator: Operator,
    config: OpenDALConfig,
}

impl OpenDALBackend {
    /// Create a new openDAL backend via dynamic types.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the `OpenDAL` backend.
    /// * `options` - Additional options for the `OpenDAL` backend.
    ///
    /// # Errors
    ///
    /// * If the path is not a valid `OpenDAL` path.
    ///
    /// # Returns
    ///
    /// A new `OpenDAL` backend.
    pub(crate) fn new(config: &OpenDALConfig) -> RusticResult<Self> {
        let mut operator = opendal::Operator::via_iter(&config.scheme, config.config.clone())
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Creating Operator from path `{path}` failed. Please check the given schema and options.",
                    err,
                )
                    .attach_context("path", config.scheme.to_string())
            })?;

        // TODO: add the extra retry options using ExponentialBackoff.
        let retry = config.retry.get_setting(constants::DEFAULT_RETRY as usize);
        operator = operator.layer(RetryLayer::new().with_max_times(retry).with_jitter());

        if let Some(x) = config.connections {
            operator = operator.layer(ConcurrentLimitLayer::new(x as usize));
        }

        if let Some(ref x) = config.throttle {
            operator = operator.layer(ThrottleLayer::new(x.bandwidth, x.burst));
        }

        let _guard = runtime().enter();
        let operator = Operator::new(operator.layer(LoggingLayer::default())).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Creating blocking Operator from path `{path}` failed.",
                err,
            )
            .attach_context("path", config.scheme.to_string())
        })?;

        Ok(Self { operator, config: config.to_owned() })
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


#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use rstest::rstest;
    use serde::Deserialize;
    use std::marker;
    use std::{fs, path::PathBuf};
    use rustic_core::RepositoryOptions;
    use crate::BackendOptions;
    // <-- fixes rstest macro expansion

    #[rstest]
    #[case("10kB,10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    #[case("10 kB,10  MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    #[case("10kB, 10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    #[case(" 10kB,   10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    #[case("10kiB,10MiB", Throttle{bandwidth:10_240, burst:10_485_760})]
    fn correct_throttle(#[case] input: &str, #[case] expected: Throttle) {
        assert_eq!(Throttle::from_str(input).unwrap(), expected);
    }

    #[rstest]
    #[case("")]
    #[case("10kiB")]
    #[case("no_number,10MiB")]
    #[case("10kB;10MB")]
    fn invalid_throttle(#[case] input: &str) {
        assert!(Throttle::from_str(input).is_err());
    }

    #[rstest]
    fn new_opendal_backend(
        #[files("tests/fixtures/opendal/*.toml")] test_case: PathBuf,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct TestCase {
            path: String,
            options: BTreeMap<String, String>,
        }

        let test: TestCase = toml::from_str(&fs::read_to_string(test_case)?)?;
        _ = OpenDALConfig::from_iter(test.path, test.options)?;
        Ok(())
    }

    /// Test `warmup_path` includes root prefix when root is configured
    #[rstest]
    #[case("s3_aws", "path/to/repo/data/")] // root = "/path/to/repo"
    #[case("s3_idrive", "data/")] // root = "/"
    fn test_warmup_path_respects_root(
        #[case] fixture: &str,
        #[case] expected_prefix: &str,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct TestCase {
            path: String,
            options: BTreeMap<String, String>,
        }

        let fixture_path = PathBuf::from(format!("tests/fixtures/opendal/{fixture}.toml"));
        let test: TestCase = toml::from_str(&fs::read_to_string(fixture_path)?)?;
        let backend = OpenDALConfig::from_iter(test.path, test.options)?;
        let be = BackendOptions::default().set_repo(&backend).to_backends()?;

        let id: Id = "03dc1178e4e54f69beaf35dd9d4256a5a600e9fa3452b9db80bd649938923e67".parse()?;
        let path = be.repository().warmup_path(FileType::Pack, &id);

        assert!(
            path.starts_with(expected_prefix),
            "warmup_path should start with '{expected_prefix}', got: {path}"
        );
        // Verify no double slashes
        assert!(
            !path.contains("//"),
            "warmup_path should not contain double slashes: {path}"
        );

        Ok(())
    }
}
