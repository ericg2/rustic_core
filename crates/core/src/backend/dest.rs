use crate::repofile::{Metadata, Node};
use crate::{ReadFileOpen, ReadSource, RestoreOptions, RusticResult, WriteFileOpen};
use bytes::Bytes;
use std::path::{Path, PathBuf};
use crate::backend::SeekFileOpen;

pub trait Destination: Send + Sync {
    /// The [`ReadSource`] to list files for this [`Destination`].
    type Iterator: ReadSource;
    type Reader: SeekFileOpen;
    type Writer: WriteFileOpen;
    
    /// Attempts to read current files in [`Destination`].
    /// 
    /// # Errors
    /// * If the path could not be read.
    /// 
    fn read_source(&self) -> RusticResult<Self::Iterator>;

    /// Remove the given directory (relative to the base path)
    ///
    /// # Arguments
    ///
    /// * `path` - The directory to remove
    ///
    /// # Errors
    ///
    /// * If the directory could not be removed.
    ///
    /// # Notes
    ///
    /// This will remove the directory recursively.
    fn remove_dir(&self, path: &Path) -> RusticResult<()>;

    /// Remove the given file (relative to the base path)
    ///
    /// # Arguments
    ///
    /// * `path` - The file to remove
    ///
    /// # Errors
    ///
    /// * If the file could not be removed.
    ///
    /// # Notes
    ///
    /// This will remove the file.
    ///
    /// * If the file is a symlink, the symlink will be removed, not the file it points to.
    /// * If the file is a directory or device, this will fail.
    fn remove_file(&self, path: &Path) -> RusticResult<()>;

    /// Create the given directory (relative to the base path)
    ///
    /// # Arguments
    ///
    /// * `item` - The directory to create
    ///
    /// # Errors
    ///
    /// * If the directory could not be created.
    ///
    /// # Notes
    ///
    /// This will create the directory structure recursively.
    fn create_dir_all(&self, path: &Path) -> RusticResult<()>;

    /// Sets the metadata for an object. This depends on the backend.
    ///
    /// # Arguments
    ///
    /// * `path` - The item to set the times for
    /// * `node` - The [`Node`] to use.
    /// * `opts` - The [`RestoreOptions`] associated.
    ///
    /// # Errors
    ///
    /// * If the times could not be set
    fn set_restore_metadata(&self, path: &Path, node: &Node, opts: &RestoreOptions) -> RusticResult<()>;

    /// Set length of `item` (relative to the base path)
    ///
    /// # Arguments
    ///
    /// * `path` - The item to set the length for
    /// * `size` - The size to set the length to
    ///
    /// # Errors
    ///
    /// * If the file does not have a parent.
    /// * If the directory could not be created.
    /// * If the file could not be opened.
    /// * If the length of the file could not be set.
    ///
    /// # Notes
    ///
    /// If the file exists, truncate it to the given length. (TODO: check if this is correct)
    /// If it doesn't exist, create a new (empty) one with given length.
    fn set_length(&self, path: &Path, size: u64) -> RusticResult<()>;

    /// Returns the file opener for a specific path.
    ///
    /// # Arguments
    ///
    /// * `path` - The item to read.
    ///
    /// # Errors
    ///
    /// * If the file does not exist.
    /// * If the file could not be opened.
    /// * If the path is a directory.
    fn get_reader(&self, path: &Path) -> RusticResult<Self::Reader>;

    /// Returns the file writer for a specific path.
    ///
    /// # Arguments
    ///
    /// * `path` - The item to write.
    ///
    /// # Errors
    ///
    /// * If the file could not be opened.
    /// * If the path is a directory.
    fn get_writer(&self, path: &Path) -> RusticResult<Self::Writer>;

    /// Check if a matching file exists.
    ///
    /// # Arguments
    ///
    /// * `item` - The item to check
    /// * `size` - The size to check
    ///
    /// # Returns
    ///
    /// If a file exists and size matches, this returns a `File` open for reading.
    /// In all other cases, returns `None`
    fn get_existing(&self, path: &Path) -> RusticResult<Option<Metadata>>;

    /// Write `data` to given item (relative to the base path) at `offset`
    ///
    /// # Arguments
    ///
    /// * `path` - The item to write to
    /// * `offset` - The offset to write at
    /// * `data` - The data to write
    ///
    /// # Errors
    ///
    /// * If the file could not be opened.
    /// * If the file could not be sought to the given position.
    /// * If the backend does not support this mode. Use [`can_random_write`] to check!
    /// * If the bytes could not be written to the file.
    ///
    /// # Notes
    ///
    /// This will create the file if it doesn't exist.
    fn write_at(&self, path: &Path, offset: u64, data: &[u8]) -> RusticResult<()>;

    /// Create a hardlink `item` pointing to `source_item`, both relative to the base path.
    ///
    /// # Arguments
    ///
    /// * `source_item` - The already-restored file to link to
    /// * `item` - The path to create as a hardlink
    ///
    /// # Errors
    ///
    /// * If the new hardlink does not have a parent directory.
    /// * If the directory could not be created.
    /// * If the hardlink could not be created.
    /// * If the backend does not support this. Use [`can_hard_link`] to check!
    fn hard_link(&self, path: &Path, item: &Path) -> RusticResult<()>;

    /// Appends `data` to given item (relative to the base path) at the end of the file.
    ///
    /// # Arguments
    ///
    /// * `path` - The item to write to
    /// * `data` - The data to write
    ///
    /// # Errors
    ///
    /// * If the file could not be opened.
    /// * If the bytes could not be written to the file.
    ///
    /// # Notes
    ///
    /// This will create the file if it doesn't exist.
    fn append(&self, path: &Path, data: &[u8]) -> RusticResult<()>;

    /// # Returns
    ///
    /// If this [`Destination`] supports random <i>writing</i>. If `false`, certain
    /// optimizations may be disabled. Speed is on a best-effort basis; however, all
    /// data will be written in a safe and correct way.
    ///
    /// # Notes
    ///
    /// Reading should always work.
    fn can_random_write(&self) -> bool;

    /// # Returns
    ///
    /// If this [`Destination`] supports hard-linking. If `false`, links in a
    /// repository are not guaranteed to be restored to this backend.
    fn can_hard_link(&self) -> bool;
}
