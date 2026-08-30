//! Module for backend related functionality.
pub(crate) mod cache;
pub(crate) mod choose;
pub(crate) mod decrypt;
pub(crate) mod dry_run;
pub(crate) mod filters;
pub(crate) mod hotcold;
pub(crate) mod list;
pub(crate) mod node;
pub(crate) mod token;
pub(crate) mod warm_up;
mod ignore;
mod command;

use bytes::Bytes;
use derive_setters::Setters;
use enum_map::Enum;
use log::trace;

#[cfg(test)]
use mockall::mock;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    Excludes, FilterOptions, RestoreOptions,
    backend::node::{Metadata, Node, NodeType},
    error::RusticResult,
    id::Id,
};

use std::{collections::HashMap, path::Path};
use std::{
    ffi::OsStr,
    io::{Read, Seek, Write},
};
use std::{fmt::Debug, io};
use std::{ops::Deref, path::PathBuf, sync::Arc};

/// All [`FileType`]s which are located in separated directories
pub const ALL_FILE_TYPES: [FileType; 4] = [
    FileType::Key,
    FileType::Snapshot,
    FileType::Index,
    FileType::Pack,
];

/// Type for describing the kind of a file that can occur.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Enum, derive_more::Display)]
pub enum FileType {
    /// Config file
    #[serde(rename = "config")]
    Config,
    /// Index
    #[serde(rename = "index")]
    Index,
    /// Keys
    #[serde(rename = "key")]
    Key,
    /// Snapshots
    #[serde(rename = "snapshot")]
    Snapshot,
    /// Data
    #[serde(rename = "pack")]
    Pack,
}

impl FileType {
    /// Returns the directory name of the file type.
    #[must_use]
    pub const fn dirname(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Snapshot => "snapshots",
            Self::Index => "index",
            Self::Key => "keys",
            Self::Pack => "data",
        }
    }

    /// Returns if the file type is cacheable.
    const fn is_cacheable(self) -> bool {
        match self {
            Self::Config | Self::Key | Self::Pack => false,
            Self::Snapshot | Self::Index => true,
        }
    }
}

// NOTE: `Node`, `NodeType`, `Metadata`, `BackendResult`, `BackendErrorKind`,
// `RusticResult`, `Excludes`, `FilterOptions`, `RestoreOptions` are assumed
// to be defined elsewhere in the crate, unchanged from the original.

/// A single entry found while listing a source.
#[derive(Debug, Clone)]
pub struct File {
    /// The path of the entry.
    pub path: PathBuf,

    /// The name of the entry.
    pub name: String,

    /// The [`NodeType`] of the entry.
    pub node_type: NodeType,

    /// The [`Metadata`] of the entry.
    pub metadata: Metadata,
}

impl File {
    /// # Returns
    /// This [`File`] as converted to a [`Node`] type.
    fn node(&self) -> Node {
        Node::new_node(
            OsStr::new(&self.name),
            self.node_type.clone(),
            self.metadata.clone(),
        )
    }

    pub fn name(&self) -> &str { &self.name }

    /// Consumes this entry, turning it into a `(path, node)` pair suitable
    /// for insertion into a tree.
    pub fn into_tree(self) -> (PathBuf, Node) {
        (self.path.clone(), self.node())
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_file(&self) -> bool {
        self.node_type == NodeType::File
    }

    pub fn is_dir(&self) -> bool {
        self.node_type == NodeType::Dir
    }

    pub fn size(&self) -> u64 {
        self.metadata.size
    }

    /// Creates a [`ReadSourceEntry`] from a given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to use. Its file name becomes the node's name.
    /// * `node_type` - The type of node (file, dir, symlink, etc).
    /// * `metadata` - Metadata for the node.
    ///
    /// # Errors
    ///
    /// Returns an error if `path` has no file name component.
    pub fn new(
        path: PathBuf,
        node_type: NodeType,
        metadata: Metadata,
    ) -> io::Result<Self> {
        let name = path
            .file_name()
            .ok_or(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Path not allowed: {}", path.display()),
            ))?
            .to_string_lossy()
            .to_string();
        Ok(Self {
            path,
            name,
            node_type,
            metadata,
        })
    }
}

/// A readable handle to a source file.
///
/// Combines [`Read`] + [`Seek`] with an explicit [`close`](ReadHandle::close)
/// step for backends that need to perform finalization work.
pub trait ReadHandle: Read + Seek + Send + Sync {
    /// Finalizes and closes the handle.
    ///
    /// # Errors
    ///
    /// Returns an error if finalizing the underlying resource fails.
    fn close(&mut self) -> io::Result<()>;
}

/// A writable handle to a destination file.
///
/// Combines [`Write`] with an explicit [`close`](WriteHandle::close) step
/// for backends that need to perform finalization work.
pub trait WriteHandle: Write + Send + Sync {
    /// Finalizes and closes the handle.
    ///
    /// # Errors
    ///
    /// Returns an error if finalizing the underlying resource fails.
    fn close(&mut self) -> io::Result<()>;
}

impl ReadHandle for std::fs::File {
    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl WriteHandle for std::fs::File {
    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Trait for backends that can list entries from a source.
///
/// # Dyn-compatibility
///
/// `Item` is fixed via the `Iterator` supertrait bound, so no per-impl
/// associated type — the trait can be used as `Box<dyn FileLister>`.
/// `close()` takes `self: Box<Self>` for the same reason.
pub trait FileLister: Iterator<Item = io::Result<File>> + Sync + Send + 'static {
    /// Returns the size of the source, if known.
    ///
    /// # Errors
    ///
    /// Returns an error if the size could not be determined.
    fn compute_size(&self) -> io::Result<Option<u64>>;

    /// Returns all root paths of the lookup.
    fn path(&self) -> &Path;
}

/// Options that control how files and directories are listed.
///
/// This configuration allows callers to exclude specific paths, apply
/// filtering rules, and choose whether the listing should recurse into
/// subdirectories.
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Setters, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
pub struct ListOptions {
    /// Optional exclusion rules used to skip matching paths during the listing.
    #[cfg_attr(feature = "clap", clap(flatten, next_help_heading = "Exclude options"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    #[serde(flatten)]
    pub excludes: Option<Excludes>,

    /// Optional filters used to determine which entries are returned.
    #[cfg_attr(feature = "clap", clap(flatten, next_help_heading = "Filter options"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    #[serde(flatten)]
    pub filters: Option<FilterOptions>,

    /// Whether to recursively traverse subdirectories.
    ///
    /// If `false`, all matching entries within the directory tree are returned.
    /// If `true`, only the immediate children of the specified directory are listed.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub no_recursive: bool,
}

/// A configuration that can be built into a [`ReadSource`].
///
/// Implement this for lightweight, serializable descriptions of a source
/// (e.g. a file path, connection string, or set of credentials) that know
/// how to construct the actual reader. `build` performs whatever I/O is
/// necessary (opening a file, connecting to a service, etc.) and returns
/// the constructed reader.
///
/// Any `ReadSourceConfig` automatically implements
/// `TryInto<Self::Reader, Error = io::Error>` (see the blanket impl below),
/// so generic code can accept either a config or a ready-made reader.
pub trait ReadSourceConfig: Serialize + DeserializeOwned + Send + Sync {
    /// The concrete [`ReadSource`] type this configuration builds.
    type Reader: ReadSource;

    /// Construct the reader described by this configuration.
    fn build(self) -> io::Result<Self::Reader>;
}

/// A configuration that can be built into a [`WriteSource`].
///
/// Mirrors [`ReadSourceConfig`] for the write path: a serializable
/// description of a destination that knows how to construct the actual
/// writer.
///
/// Any `WriteSourceConfig` automatically implements
/// `TryInto<Self::Writer, Error = io::Error>` (see the blanket impl below),
/// so generic code can accept either a config or a ready-made writer.
pub trait WriteSourceConfig: Serialize + DeserializeOwned + Send + Sync {
    /// The concrete [`WriteSource`] type this configuration builds.
    type Writer: WriteSource;

    /// Construct the writer described by this configuration.
    fn build(self) -> io::Result<Self::Writer>;
}

/// Trait for a backend that can be read from as a source of files.
///
/// # Dyn-compatibility
///
/// `Lister` and `Reader` associated types are replaced with
/// `Box<dyn FileLister>` and `Box<dyn ReadHandle>`, so `dyn ReadSource` can
/// be used directly.
pub trait ReadSource: Send + Sync + 'static {
    /// Returns a human-readable location string for this backend, used for
    /// logging and error messages.
    fn location(&self) -> String;

    /// Opens `path` for reading.
    ///
    /// # Errors
    ///
    /// Returns an error if the file could not be opened.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadHandle>>;

    /// Lists all items in the current level. Non-recursive.
    ///
    /// # Errors
    /// Returns an error if the source listing failed.
    fn readdir(&self, path: &Path) -> io::Result<Box<dyn Iterator<Item = io::Result<Node>> + Send>>;

    /// Returns metadata for `path`, if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata could not be retrieved for a
    /// reason other than the path not existing.
    fn stat(&self, path: &Path) -> io::Result<Option<Metadata>>;

    /// Returns if the file exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata could not be retrieved for a
    /// reason other than the path not existing.
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.stat(path).map(|x| x.is_some())
    }
}

/// Trait for a backend that can be written to as a restore destination.
///
/// Extends [`ReadSource`]; `Writer` associated type replaced with
/// `Box<dyn WriteHandle>` so `dyn WriteSource` is usable directly.
pub trait WriteSource: ReadSource + Send + Sync + 'static {
    /// Removes the given directory (relative to the base path), including
    /// all of its contents, recursively.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory could not be removed.
    fn remove_dir(&self, path: &Path) -> io::Result<()>;

    /// Removes the given file (relative to the base path).
    ///
    /// If the file is a symlink, only the symlink itself is removed. If the
    /// path refers to a directory or device, this will fail.
    ///
    /// # Errors
    ///
    /// Returns an error if the file could not be removed.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Creates the given directory (relative to the base path), including
    /// all necessary parent directories.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory could not be created.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Sets restore-related metadata (timestamps, permissions, etc.) for an
    /// object. Exact semantics depend on the backend.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata could not be set.
    fn set_restore_metadata(
        &self,
        path: &Path,
        node: &Node,
        opts: &RestoreOptions,
    ) -> io::Result<()>;

    /// Sets the length of `path` (relative to the base path). Truncates or
    /// extends an existing file, or creates a new empty file of that
    /// length if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory is missing/uncreatable, the
    /// file could not be opened, or the length could not be set.
    fn set_length(&self, path: &Path, size: u64) -> io::Result<()>;

    /// Opens `path` for writing (replacing its contents).
    ///
    /// # Errors
    ///
    /// Returns an error if the file could not be opened, or if `path`
    /// refers to a directory.
    fn open_replace(&self, path: &Path) -> io::Result<Box<dyn WriteHandle>>;

    /// Writes all bytes in an atomic way.
    fn write_all(&self, path: &Path, bytes: Bytes) -> io::Result<()>;

    /// Writes `data` to `path` at the given byte `offset`. Creates the file
    /// if it does not already exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file could not be opened/sought, the
    /// backend does not support random writes (check
    /// [`can_random_write`](WriteSource::can_random_write) first), or the
    /// data could not be written.
    fn write_at(&self, path: &Path, offset: u64, data: &[u8]) -> io::Result<()>;

    /// Creates a hardlink at `item` pointing to the already-restored file
    /// at `source_item`, both relative to the base path.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent directory is missing/uncreatable,
    /// the hardlink could not be created, or the backend does not support
    /// hardlinks (check [`can_hard_link`](WriteSource::can_hard_link)
    /// first).
    fn hard_link(&self, path: &Path, item: &Path) -> io::Result<()>;

    /// Returns whether this destination supports random-access writes.
    fn can_random_write(&self) -> bool;

    /// Returns whether this destination supports hard-linking.
    fn can_hard_link(&self) -> bool;
}

/// Trait for backends that can read.
///
/// This trait is implemented by all backends that can read data.
///
/// # Dyn-compatibility
///
/// This trait deliberately has **no associated types**. It used to declare
/// `type Source: ReadSource;`, but that type was unused anywhere in the
/// trait's own methods and its presence made `dyn ReadBackend` (and by
/// extension `dyn WriteBackend`, which extends it and is used throughout
/// this module as `Arc<dyn WriteBackend>`) impossible to construct. Removing
/// it costs nothing functionally and restores dyn-compatibility.
pub trait ReadBackend: Send + Sync + 'static {
    /// Returns a human-readable location string for this backend, used for
    /// logging, error messages, and the [`Debug`] impl of `dyn WriteBackend`.
    fn location(&self) -> String;

    /// Lists all files with their size of the given type.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the files to list.
    ///
    /// # Errors
    ///
    /// * If the files could not be listed.
    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>>;

    /// Lists all files of the given type.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the files to list.
    ///
    /// # Errors
    ///
    /// * If the files could not be listed.
    fn list(&self, tpe: FileType) -> RusticResult<Vec<Id>> {
        Ok(self
            .list_with_size(tpe)?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    /// Reads full data of the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    ///
    /// # Errors
    ///
    /// * If the file could not be read.
    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes>;

    /// Reads partial data of the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `cacheable` - Whether the file should be cached.
    /// * `offset` - The offset to read from.
    /// * `length` - The length to read.
    ///
    /// # Errors
    ///
    /// * If the file could not be read.
    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes>;

    /// Get the warmup path for the given file type and id.
    ///
    /// This method returns a string representing the backend-specific path or identifier
    /// for a file, which can be used as input to external warm-up commands. Unlike a
    /// hypothetical `path()` method which may have different return types for different
    /// backends, this method must always return a string that can be passed to external
    /// programs.
    ///
    /// This is primarily used for warming up files in cold storage before they are
    /// accessed, where the warm-up command needs to know the specific backend path
    /// or identifier to request from the storage service.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    ///
    /// # Returns
    ///
    /// A string containing the backend-specific path or identifier for the file.
    fn warmup_path(&self, tpe: FileType, id: &Id) -> String;

    /// Specify if the backend needs a warming-up of files before accessing them.
    fn needs_warm_up(&self) -> bool {
        false
    }

    /// Warm-up the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    ///
    /// # Errors
    ///
    /// * If the file could not be read.
    fn warm_up(&self, _tpe: FileType, _id: &Id) -> RusticResult<()> {
        Ok(())
    }
}

/// Trait for Searching in a backend.
///
/// This trait is implemented by all backends that can be searched in.
///
/// # Note
///
/// This trait is used to find the id of a snapshot that contains a given file name.
///
/// This trait uses generic methods, so it is not itself dyn-compatible.
/// That's fine: it's only ever used via the blanket `impl<T: ReadBackend>
/// FindInBackend for T` below on concrete types, never as `dyn
/// FindInBackend`.
pub trait FindInBackend: ReadBackend {
    /// Finds the id of the file starting with the given string.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The type of the strings.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `vec` - The strings to search for.
    ///
    /// # Errors
    ///
    /// * If no id could be found.
    /// * If the id is not unique.
    ///
    /// # Note
    ///
    /// This function is used to find the id of a snapshot.
    fn find_starts_with<T: AsRef<str>>(&self, tpe: FileType, vec: &[T]) -> RusticResult<Vec<Id>> {
        Id::find_starts_with_from_iter(vec, self.list(tpe)?)
    }

    /// Finds the id of the file starting with the given string.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The string to search for.
    ///
    /// # Errors
    ///
    /// * If the string is not a valid hexadecimal string
    /// * If no id could be found.
    /// * If the id is not unique.
    fn find_id(&self, tpe: FileType, id: &str) -> RusticResult<Id> {
        Ok(self.find_ids(tpe, &[id.to_string()])?.remove(0))
    }

    /// Finds the ids of the files starting with the given strings.
    ///
    /// # Type Parameters
    ///
    /// * `T` - The type of the strings.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `ids` - The strings to search for.
    ///
    /// # Errors
    ///
    /// * If the string is not a valid hexadecimal string
    /// * If no id could be found.
    /// * If the id is not unique.
    fn find_ids<T: AsRef<str>>(&self, tpe: FileType, ids: &[T]) -> RusticResult<Vec<Id>> {
        ids.iter()
            .map(|id| id.as_ref().parse())
            .collect::<RusticResult<Vec<_>>>()
            .or_else(|err|{
                trace!("no valid IDs given: {err}, searching for ID starting with given strings instead");
                self.find_starts_with(tpe, ids)})
    }
}

impl<T: ReadBackend> FindInBackend for T {}

/// Trait for backends that can write.
/// This trait is implemented by all backends that can write data.
pub trait WriteBackend: ReadBackend {
    /// Creates a new backend.
    ///
    /// # Errors
    ///
    /// * If the backend could not be created.
    ///
    /// # Returns
    ///
    /// The result of the creation.
    fn create(&self) -> RusticResult<()> {
        Ok(())
    }

    /// Writes bytes to the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `cacheable` - Whether the data can be cached.
    /// * `buf` - The data to write.
    ///
    /// # Errors
    ///
    /// * If the data could not be written.
    ///
    /// # Returns
    ///
    /// The result of the write.
    fn write_bytes(&self, tpe: FileType, id: &Id, cacheable: bool, buf: Bytes) -> RusticResult<()>;

    /// Removes the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `cacheable` - Whether the file is cacheable.
    ///
    /// # Errors
    ///
    /// * If the file could not be removed.
    ///
    /// # Returns
    ///
    /// The result of the removal.
    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()>;
}

#[cfg(test)]
mock! {
    pub(crate) Backend {}

    impl ReadBackend for Backend {
        fn location(&self) -> String;
        fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>>;
        fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes>;
        fn read_partial(
            &self,
            tpe: FileType,
            id: &Id,
            cacheable: bool,
            offset: u32,
            length: u32,
        ) -> RusticResult<Bytes>;
        fn warmup_path(&self, tpe: FileType, id: &Id) -> String;
    }

    impl WriteBackend for Backend {
        fn create(&self) -> RusticResult<()>;
        fn write_bytes(&self, tpe: FileType, id: &Id, cacheable: bool, buf: Bytes) -> RusticResult<()>;
        fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()>;
    }
}

impl WriteBackend for Arc<dyn WriteBackend> {
    fn create(&self) -> RusticResult<()> {
        self.deref().create()
    }
    fn write_bytes(&self, tpe: FileType, id: &Id, cacheable: bool, buf: Bytes) -> RusticResult<()> {
        self.deref().write_bytes(tpe, id, cacheable, buf)
    }
    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()> {
        self.deref().remove(tpe, id, cacheable)
    }
}

impl ReadBackend for Arc<dyn WriteBackend> {
    fn location(&self) -> String {
        self.deref().location()
    }
    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        self.deref().list_with_size(tpe)
    }
    fn list(&self, tpe: FileType) -> RusticResult<Vec<Id>> {
        self.deref().list(tpe)
    }
    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        self.deref().read_full(tpe, id)
    }
    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes> {
        self.deref()
            .read_partial(tpe, id, cacheable, offset, length)
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        self.deref().warmup_path(tpe, id)
    }
    fn needs_warm_up(&self) -> bool {
        self.deref().needs_warm_up()
    }
    fn warm_up(&self, tpe: FileType, id: &Id) -> RusticResult<()> {
        self.deref().warm_up(tpe, id)
    }
}

impl Debug for dyn WriteBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WriteBackend{{{}}}", self.location())
    }
}

// /// Trait for backends that can start a read.
// pub trait ReadSourceConfig:
//     serde::Serialize + DeserializeOwned + Debug + Sync + Send + Default + 'static
// {
//     /// The [`ReadSource`] to create from this builder.
//     type Reader: FileLister;

//     /// Opens a [`ReadSource`] for the specified path and options.
//     ///
//     /// # Errors
//     ///
//     /// If the backend fails to initialize reading.
//     fn get_reader(&self) -> RusticResult<Self::Reader>;
// }

/// Trait for repository backends.
pub trait BackendConfig: Serialize + DeserializeOwned + Clone + Debug + Send + Sync {
    /// The [`WriteBackend`] returned by this config.
    type Output: WriteBackend;

    /// Creates the [`BackendConfig`] from an iterator.
    ///
    /// # Important
    /// This does not guarantee the [`BackendConfig`] is initialized correctly. Due to the
    /// nature of dynamic types - this feature is only a convenience. All invalid fields will
    /// be skipped, and will not return an error during the process.
    fn from_iter<K, V, I>(path: impl AsRef<str>, dict: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>;

    /// # Returns
    ///
    /// A string [`Path`] of the [`BackendConfig`].
    fn get_path(&self) -> Option<String>;

    /// # Returns
    ///
    /// All dynamic options of this [`BackendConfig`]. It should de-serialize correctly.
    fn get_options(&self) -> HashMap<String, String>;

    /// # Returns
    ///
    /// The [`Repository`] backend for this config.
    ///
    /// # Errors
    /// * If the backend could not be created.
    /// * If the configuration is invalid.
    fn get_repo(&self) -> RusticResult<Self::Output>;
}

/// The backends a repository can be initialized and operated on
///
/// # Note
///
/// This struct is used to initialize a [`Repository`](crate::Repository).
///
#[derive(Debug, Clone)]
pub struct RepositoryBackends {
    /// The main repository of this [`RepositoryBackends`].
    repository: Arc<dyn WriteBackend>,

    /// The hot repository of this [`RepositoryBackends`].
    repo_hot: Option<Arc<dyn WriteBackend>>,
}

impl RepositoryBackends {
    /// Creates a new [`RepositoryBackends`].
    ///
    /// # Arguments
    ///
    /// * `repository` - The main repository of this [`RepositoryBackends`].
    /// * `repo_hot` - The hot repository of this [`RepositoryBackends`].
    pub fn new(repository: Arc<dyn WriteBackend>, repo_hot: Option<Arc<dyn WriteBackend>>) -> Self {
        Self {
            repository,
            repo_hot,
        }
    }

    /// Returns the repository of this [`RepositoryBackends`].
    #[must_use]
    pub fn repository(&self) -> Arc<dyn WriteBackend> {
        self.repository.clone()
    }

    /// Returns the hot repository of this [`RepositoryBackends`].
    #[must_use]
    pub fn repo_hot(&self) -> Option<Arc<dyn WriteBackend>> {
        self.repo_hot.clone()
    }
}
