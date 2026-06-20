#[cfg(not(windows))]
use std::os::unix::fs::{PermissionsExt, symlink};

use std::{
    fs::{self, OpenOptions},
    io,
    io::{Seek, SeekFrom, Write},
    num::TryFromIntError,
    path::{Path, PathBuf},
};

use crate::local::LocalSource;
#[cfg(not(windows))]
use crate::local::mapper::nix_mapper::map_mode_from_go;
use crate::local::source::{LocalFile, LocalReader};
use derive_setters::Setters;
use filetime::{FileTime, set_symlink_file_times};
use jiff::Timestamp;
use log::{debug, warn};
#[cfg(not(windows))]
use nix::errno::Errno;
#[cfg(not(windows))]
use nix::sys::stat::{Mode, SFlag, mknod};
#[cfg(not(windows))]
use nix::{
    fcntl::{AT_FDCWD, AtFlags},
    unistd::{Gid, Uid, fchownat},
};
use rustic_core::repofile::{Metadata, Node};
use rustic_core::{
    Destination, DestinationBuilder, ErrorKind, ExtendedAttribute, ReadSourceBuilder,
    RestoreOptions, RusticError, RusticResult,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[cfg(not(windows))]
mod helpers {
    // Helper function to cache mapping user name -> uid
    #![allow(clippy::needless_pass_by_value)]

    use cached::macros::cached;
    use log::warn;
    use nix::unistd::{Gid, Group, Uid, User};

    #[cached]
    pub(super) fn uid_from_name(name: String) -> Option<Uid> {
        User::from_name(&name)
            .inspect_err(|err| warn!("Cannot determine UID from name {name}: {err}. Using UID 0."))
            .unwrap_or_default()
            .map(|u| u.uid)
    }

    // Helper function to cache mapping group name -> gid
    #[cached]
    pub(super) fn gid_from_name(name: String) -> Option<Gid> {
        Group::from_name(&name)
            .inspect_err(|err| warn!("Cannot determine GID from name {name}: {err}. Using UID 0."))
            .unwrap_or_default()
            .map(|g| g.gid)
    }
}

/// [`LocalDestinationErrorKind`] describes the errors that can be returned by an action on the filesystem in Backends
#[derive(thiserror::Error, Debug, displaydoc::Display)]
pub enum LocalDestinationErrorKind {
    /// directory creation failed: `{0:?}`
    DirectoryCreationFailed(io::Error),

    #[cfg(any(
        target_os = "macos",
        target_os = "openbsd",
        all(target_os = "android", target_pointer_width = "32")
    ))]
    /// `DeviceID` could not be converted to other type `{target}` of device `{device}`: `{source}`
    DeviceIdConversionFailed {
        target: String,
        device: u64,
        source: TryFromIntError,
    },
    /// Length conversion failed for `{target}` of length `{length}`: `{source}`
    LengthConversionFailed {
        target: String,
        length: u64,
        source: TryFromIntError,
    },
    /// `{0}`
    #[error(transparent)]
    #[cfg(not(windows))]
    FromErrnoError(Errno),
    /// listing xattrs on `{path:?}`: `{source:?}`
    #[cfg(not(any(windows, target_os = "openbsd")))]
    ListingXattrsFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    /// setting xattr `{name}` on `{filename:?}` with `{source:?}`
    #[cfg(not(any(windows, target_os = "openbsd")))]
    SettingXattrFailed {
        name: String,
        filename: PathBuf,
        source: std::io::Error,
    },
    /// getting xattr `{name}` on `{filename:?}` with `{source:?}`
    #[cfg(not(any(windows, target_os = "openbsd")))]
    GettingXattrFailed {
        name: String,
        filename: PathBuf,
        source: std::io::Error,
    },
    /// removing directories failed: `{0:?}`
    DirectoryRemovalFailed(io::Error),
    /// removing file failed: `{0:?}`
    FileRemovalFailed(io::Error),
    /// setting time metadata failed: `{0:?}`
    SettingTimeMetadataFailed(io::Error),
    /// opening file failed: `{0:?}`
    OpeningFileFailed(io::Error),
    /// setting file length failed: `{0:?}`
    SettingFileLengthFailed(io::Error),
    /// can't jump to position in file: `{0:?}`
    CouldNotSeekToPositionInFile(io::Error),
    /// couldn't write to buffer: `{0:?}`
    CouldNotWriteToBuffer(io::Error),
    /// reading exact length of file contents failed: `{0:?}`
    ReadingExactLengthOfFileFailed(io::Error),
    /// setting file permissions failed: `{0:?}`
    #[cfg(not(windows))]
    SettingFilePermissionsFailed(std::io::Error),
    /// failed to symlink target `{linktarget:?}` from `{filename:?}` with `{source:?}`
    #[cfg(not(windows))]
    SymlinkingFailed {
        linktarget: PathBuf,
        filename: PathBuf,
        source: std::io::Error,
    },
    /// failed to create hardlink from `{source_path:?}` to `{filename:?}` with `{source:?}`
    HardLinkingFailed {
        source_path: PathBuf,
        filename: PathBuf,
        source: io::Error,
    },
}

pub(crate) type LocalDestinationResult<T> = Result<T, LocalDestinationErrorKind>;

/// Local destination, used when restoring.
#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct LocalDestination {
    /// The base path of the destination.
    pub path: Option<PathBuf>,
}

impl LocalDestination {
    /// Create a new [`LocalDestination`]. The path will be created if not existing.
    ///
    /// # Arguments
    ///
    /// * `path` - The base path of the destination
    ///
    /// # Errors
    ///
    /// * If the directory could not be created.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_path_buf()),
        }
    }
}

impl DestinationBuilder for LocalDestination {
    type Output = LocalWriter;

    fn get_destination(&self) -> RusticResult<Self::Output> {
        let path = self.path.as_ref().ok_or(RusticError::new(
            ErrorKind::Configuration,
            "Root is required for source.",
        ))?;
        let ret = LocalWriter::new(path.clone());
        Ok(ret)
    }
}

#[derive(Clone, Debug)]
pub struct LocalWriter {
    path: PathBuf,
}

impl LocalWriter {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Set changed and modified times for `item` (relative to the base path) utilizing the file metadata
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the times for
    /// * `meta` - The metadata to get the times from
    ///
    /// # Errors
    ///
    /// * If the times could not be set
    pub(crate) fn set_times(
        &self,
        item: impl AsRef<Path>,
        meta: &Metadata,
    ) -> LocalDestinationResult<()> {
        let filename = self.path(item.as_ref());
        if let Some(mtime) = meta.mtime {
            let atime = meta.atime.unwrap_or(mtime);
            set_symlink_file_times(
                filename,
                FileTime::from_system_time(atime.into()),
                FileTime::from_system_time(mtime.into()),
            )
            .map_err(LocalDestinationErrorKind::SettingTimeMetadataFailed)?;
        }

        Ok(())
    }

    #[cfg(windows)]
    // TODO: Windows support
    /// Set user/group for `item` (relative to the base path) utilizing the file metadata
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the user/group for
    /// * `meta` - The metadata to get the user/group from
    ///
    /// # Errors
    ///
    /// * If the user/group could not be set.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    pub(crate) fn set_user_group(
        &self,
        _item: impl AsRef<Path>,
        _meta: &Metadata,
    ) -> LocalDestinationResult<()> {
        // https://learn.microsoft.com/en-us/windows/win32/fileio/file-security-and-access-rights
        // https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Security/struct.SECURITY_ATTRIBUTES.html
        // https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Storage/FileSystem/struct.CREATEFILE2_EXTENDED_PARAMETERS.html#structfield.lpSecurityAttributes
        Ok(())
    }

    #[cfg(not(windows))]
    /// Set user/group for `item` (relative to the base path) utilizing the file metadata
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the user/group for
    /// * `meta` - The metadata to get the user/group from
    ///
    /// # Errors
    ///
    /// * If the user/group could not be set.
    #[allow(clippy::similar_names)]
    pub(crate) fn set_user_group(
        &self,
        item: impl AsRef<Path>,
        meta: &Metadata,
    ) -> LocalDestinationResult<()> {
        let filename = self.path(item);

        let user = meta.user.clone().and_then(helpers::uid_from_name);
        // use uid from user if valid, else from saved uid (if saved)
        let uid = user.or_else(|| meta.uid.map(Uid::from_raw));

        let group = meta.group.clone().and_then(helpers::gid_from_name);
        // use gid from group if valid, else from saved gid (if saved)
        let gid = group.or_else(|| meta.gid.map(Gid::from_raw));

        fchownat(AT_FDCWD, &filename, uid, gid, AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(LocalDestinationErrorKind::FromErrnoError)?;
        Ok(())
    }

    #[cfg(windows)]
    // TODO: Windows support
    /// Set uid/gid for `item` (relative to the base path) utilizing the file metadata
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the uid/gid for
    /// * `meta` - The metadata to get the uid/gid from
    ///
    /// # Errors
    ///
    /// * If the uid/gid could not be set.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    pub(crate) fn set_uid_gid(
        &self,
        _item: impl AsRef<Path>,
        _meta: &Metadata,
    ) -> LocalDestinationResult<()> {
        Ok(())
    }

    #[cfg(not(windows))]
    /// Set uid/gid for `item` (relative to the base path) utilizing the file metadata
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the uid/gid for
    /// * `meta` - The metadata to get the uid/gid from
    ///
    /// # Errors
    ///
    /// * If the uid/gid could not be set.
    #[allow(clippy::similar_names)]
    pub(crate) fn set_uid_gid(
        &self,
        item: impl AsRef<Path>,
        meta: &Metadata,
    ) -> LocalDestinationResult<()> {
        let filename = self.path(item);

        let uid = meta.uid.map(Uid::from_raw);
        let gid = meta.gid.map(Gid::from_raw);

        fchownat(AT_FDCWD, &filename, uid, gid, AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(LocalDestinationErrorKind::FromErrnoError)?;
        Ok(())
    }

    #[cfg(windows)]
    // TODO: Windows support
    /// Set permissions for `item` (relative to the base path) from `node`
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the permissions for
    /// * `node` - The node to get the permissions from
    ///
    /// # Errors
    ///
    /// * If the permissions could not be set.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    pub(crate) fn set_permission(
        &self,
        _item: impl AsRef<Path>,
        _node: &Node,
    ) -> LocalDestinationResult<()> {
        Ok(())
    }

    #[cfg(not(windows))]
    /// Set permissions for `item` (relative to the base path) from `node`
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the permissions for
    /// * `node` - The node to get the permissions from
    ///
    /// # Errors
    ///
    /// * If the permissions could not be set.
    #[allow(clippy::similar_names)]
    pub(crate) fn set_permission(
        &self,
        item: impl AsRef<Path>,
        node: &Node,
    ) -> LocalDestinationResult<()> {
        if node.is_symlink() {
            return Ok(());
        }

        let filename = self.path(item);

        if let Some(mode) = node.meta.mode {
            let mode = map_mode_from_go(mode);
            fs::set_permissions(filename, fs::Permissions::from_mode(mode))
                .map_err(LocalDestinationErrorKind::SettingFilePermissionsFailed)?;
        }
        Ok(())
    }

    #[cfg(any(windows, target_os = "openbsd"))]
    // TODO: Windows support
    // TODO: openbsd support
    /// Set extended attributes for `item` (relative to the base path)
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the extended attributes for
    /// * `extended_attributes` - The extended attributes to set
    ///
    /// # Errors
    ///
    /// * If the extended attributes could not be set.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    pub(crate) fn set_extended_attributes(
        &self,
        _item: impl AsRef<Path>,
        _extended_attributes: &[ExtendedAttribute],
    ) -> LocalDestinationResult<()> {
        Ok(())
    }

    #[cfg(not(any(windows, target_os = "openbsd")))]
    /// Set extended attributes for `item` (relative to the base path)
    ///
    /// # Arguments
    ///
    /// * `item` - The item to set the extended attributes for
    /// * `extended_attributes` - The extended attributes to set
    ///
    /// # Errors
    ///
    /// * If listing the extended attributes failed.
    /// * If getting an extended attribute failed.
    /// * If setting an extended attribute failed.
    ///
    /// # Returns
    ///
    /// Ok if the extended attributes were set.
    ///
    /// # Panics
    ///
    /// * If the extended attributes could not be set.
    pub(crate) fn set_extended_attributes(
        &self,
        item: impl AsRef<Path>,
        extended_attributes: &[ExtendedAttribute],
    ) -> LocalDestinationResult<()> {
        let filename = self.path(item);
        let mut done = vec![false; extended_attributes.len()];

        for curr_name in xattr::list(&filename).map_err(|err| {
            LocalDestinationErrorKind::ListingXattrsFailed {
                source: err,
                path: filename.clone(),
            }
        })? {
            match extended_attributes.iter().enumerate().find(
                |(_, ExtendedAttribute { name, .. })| name == curr_name.to_string_lossy().as_ref(),
            ) {
                Some((index, ExtendedAttribute { name, value })) => {
                    let curr_value = xattr::get(&filename, name).map_err(|err| {
                        LocalDestinationErrorKind::GettingXattrFailed {
                            name: name.clone(),
                            filename: filename.clone(),
                            source: err,
                        }
                    })?;
                    if value != &curr_value {
                        xattr::set(&filename, name, value.as_ref().unwrap_or(&Vec::new()))
                            .map_err(|err| LocalDestinationErrorKind::SettingXattrFailed {
                                name: name.clone(),
                                filename: filename.clone(),
                                source: err,
                            })?;
                    }
                    done[index] = true;
                }
                None => {
                    if let Err(err) = xattr::remove(&filename, &curr_name) {
                        warn!(
                            "error removing xattr {} on {}: {err}",
                            curr_name.display(),
                            filename.display()
                        );
                    }
                }
            }
        }

        for (index, ExtendedAttribute { name, value }) in extended_attributes.iter().enumerate() {
            if !done[index] {
                xattr::set(&filename, name, value.as_ref().unwrap_or(&Vec::new())).map_err(
                    |err| LocalDestinationErrorKind::SettingXattrFailed {
                        name: name.clone(),
                        filename: filename.clone(),
                        source: err,
                    },
                )?;
            }
        }

        Ok(())
    }

    #[cfg(windows)]
    // TODO: Windows support
    /// Create a special file (relative to the base path)
    ///
    /// # Arguments
    ///
    /// * `item` - The item to create
    /// * `node` - The node to get the type from
    ///
    /// # Errors
    ///
    /// * If the special file could not be created.
    ///
    /// # Returns
    ///
    /// Ok if the special file was created.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    pub(crate) fn create_special(
        &self,
        _item: impl AsRef<Path>,
        _node: &Node,
    ) -> LocalDestinationResult<()> {
        Ok(())
    }

    #[cfg(not(windows))]
    /// Create a special file (relative to the base path)
    ///
    /// # Arguments
    ///
    /// * `item` - The item to create
    /// * `node` - The node to get the type from
    ///
    /// # Errors
    ///
    /// * If the symlink could not be created.
    /// * If the device could not be converted to the correct type.
    /// * If the device could not be created.
    pub(crate) fn create_special(
        &self,
        item: impl AsRef<Path>,
        node: &Node,
    ) -> LocalDestinationResult<()> {
        let filename = self.path(item);

        match &node.node_type {
            NodeType::Symlink { .. } => {
                let linktarget = node.node_type.to_link();
                symlink(linktarget, &filename).map_err(|err| {
                    LocalDestinationErrorKind::SymlinkingFailed {
                        linktarget: linktarget.to_path_buf(),
                        filename,
                        source: err,
                    }
                })?;
            }
            NodeType::Dev { device } => {
                #[cfg(not(any(
                    target_os = "macos",
                    target_os = "openbsd",
                    all(target_os = "android", target_pointer_width = "32")
                )))]
                let device = *device;
                #[cfg(any(target_os = "macos", target_os = "openbsd"))]
                let device = i32::try_from(*device).map_err(|err| {
                    LocalDestinationErrorKind::DeviceIdConversionFailed {
                        target: "i32".to_string(),
                        device: *device,
                        source: err,
                    }
                })?;
                #[cfg(all(target_os = "android", target_pointer_width = "32"))]
                let device = u32::try_from(*device).map_err(|err| {
                    LocalDestinationErrorKind::DeviceIdConversionFailed {
                        target: "u32".to_string(),
                        device: *device,
                        source: err,
                    }
                })?;
                mknod(&filename, SFlag::S_IFBLK, Mode::empty(), device)
                    .map_err(LocalDestinationErrorKind::FromErrnoError)?;
            }
            NodeType::Chardev { device } => {
                #[cfg(not(any(
                    target_os = "macos",
                    target_os = "openbsd",
                    all(target_os = "android", target_pointer_width = "32")
                )))]
                let device = *device;
                #[cfg(any(target_os = "macos", target_os = "openbsd"))]
                let device = i32::try_from(*device).map_err(|err| {
                    LocalDestinationErrorKind::DeviceIdConversionFailed {
                        target: "i32".to_string(),
                        device: *device,
                        source: err,
                    }
                })?;
                #[cfg(all(target_os = "android", target_pointer_width = "32"))]
                let device = u32::try_from(*device).map_err(|err| {
                    LocalDestinationErrorKind::DeviceIdConversionFailed {
                        target: "u32".to_string(),
                        device: *device,
                        source: err,
                    }
                })?;
                mknod(&filename, SFlag::S_IFCHR, Mode::empty(), device)
                    .map_err(LocalDestinationErrorKind::FromErrnoError)?;
            }
            NodeType::Fifo => {
                mknod(&filename, SFlag::S_IFIFO, Mode::empty(), 0)
                    .map_err(LocalDestinationErrorKind::FromErrnoError)?;
            }
            NodeType::Socket => {
                mknod(&filename, SFlag::S_IFSOCK, Mode::empty(), 0)
                    .map_err(LocalDestinationErrorKind::FromErrnoError)?;
            }
            _ => {}
        }
        Ok(())
    }
}

impl LocalWriter {
    fn path(&self, path: &Path) -> PathBuf {
        crate::join_force(&self.path, path)
    }
}

impl Destination for LocalWriter {
    type Iterator = LocalReader;
    type Reader = LocalFile;
    type Writer = LocalFile;

    fn read_source(&self) -> RusticResult<Self::Iterator> {
        LocalSource::new(&self.path).get_reader()
    }

    fn remove_dir(&self, path: &Path) -> RusticResult<()> {
        let path = &self.path(path);
        fs::remove_dir_all(path).map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Failed to remove directory", err)
        })?;
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> RusticResult<()> {
        let path = &self.path(path);
        fs::remove_file(path).map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Failed to remove file", err)
        })?;
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> RusticResult<()> {
        let path = &self.path(path);
        fs::create_dir_all(path).map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Failed to create directory", err)
        })?;
        Ok(())
    }

    fn set_restore_metadata(
        &self,
        path: &Path,
        node: &Node,
        opts: &RestoreOptions,
    ) -> RusticResult<()> {
        // This does not use 'self.path()' due to all self.X functions already doing it.
        debug!("setting metadata for {}", path.display());
        self.create_special(path, node)
            .unwrap_or_else(|_| warn!("restore {}: creating special file failed.", path.display()));
        match (opts.no_ownership, opts.numeric_id) {
            (true, _) => {}
            (false, true) => self
                .set_uid_gid(path, &node.meta)
                .unwrap_or_else(|_| warn!("restore {}: setting UID/GID failed.", path.display())),
            (false, false) => self.set_user_group(path, &node.meta).unwrap_or_else(|_| {
                warn!("restore {}: setting User/Group failed.", path.display())
            }),
        }
        self.set_permission(path, node)
            .unwrap_or_else(|_| warn!("restore {}: chmod failed.", path.display()));
        self.set_extended_attributes(path, &node.meta.extended_attributes)
            .unwrap_or_else(|_| {
                warn!(
                    "restore {}: setting extended attributes failed.",
                    path.display()
                );
            });
        self.set_times(path, &node.meta)
            .unwrap_or_else(|_| warn!("restore {}: setting file times failed.", path.display()));
        Ok(())
    }

    fn set_length(&self, path: &Path, size: u64) -> RusticResult<()> {
        let filename = self.path(path);
        let ret = (|| -> io::Result<()> {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(filename)?
                .set_len(size)?;
            Ok(())
        })();
        ret.map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Failed to set length of file", err)
        })
    }

    fn get_reader(&self, path: &Path) -> RusticResult<Self::Reader> {
        Ok(LocalFile(self.path(path)))
    }

    fn get_writer(&self, path: &Path) -> RusticResult<Self::Writer> {
        Ok(LocalFile(self.path(path)))
    }

    fn get_existing(&self, path: &Path) -> RusticResult<Option<Metadata>> {
        let filename = self.path(path);
        let meta = match fs::symlink_metadata(&filename) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(RusticError::with_source(
                    ErrorKind::Backend,
                    "Failed to read metadata",
                    err,
                ));
            }
        };

        let ret = Metadata {
            mode: None,
            mtime: meta
                .modified()
                .ok()
                .and_then(|x| Timestamp::try_from(x).ok()),
            atime: meta
                .accessed()
                .ok()
                .and_then(|x| Timestamp::try_from(x).ok()),
            ctime: meta
                .created()
                .ok()
                .and_then(|x| Timestamp::try_from(x).ok()),
            uid: None,
            gid: None,
            user: None,
            group: None,
            inode: 0,
            device_id: 0,
            size: meta.len(),
            links: 0,
            extended_attributes: vec![],
        };

        Ok(Some(ret))
    }

    fn write_at(&self, path: &Path, offset: u64, data: &[u8]) -> RusticResult<()> {
        let filename = self.path(path);
        let ret = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(filename)?;

            let _ = file.seek(SeekFrom::Start(offset))?;
            file.write_all(data)?;
            Ok(())
        })();
        ret.map_err(|err| RusticError::with_source(ErrorKind::Backend, "Failed to write file", err))
    }

    fn hard_link(&self, source_item: &Path, item: &Path) -> RusticResult<()> {
        let source_path = self.path(source_item);
        let filename = self.path(item);
        let ret = (|| -> io::Result<()> {
            fs::hard_link(&source_path, &filename)?;
            Ok(())
        })();
        ret.map_err(|err| RusticError::with_source(ErrorKind::Backend, "Failed to hard link", err))
    }

    fn append(&self, path: &Path, data: &[u8]) -> RusticResult<()> {
        let filename = self.path(path);
        let ret = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(filename)?;
            file.write_all(data)?;
            Ok(())
        })();
        ret.map_err(|err| RusticError::with_source(ErrorKind::Backend, "Failed to append", err))
    }

    fn can_random_write(&self) -> bool {
        true
    }

    fn can_hard_link(&self) -> bool {
        true
    }
}
