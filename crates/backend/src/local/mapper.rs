#[cfg(not(windows))]
pub mod nix_mapper;

use std::{ffi::OsStr, path::Path};

use derive_setters::Setters;
use ignore::DirEntry;
use jiff::Timestamp;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::local::lister::{IgnoreErrorKind, IgnoreResult, LocalFile};

use rustic_core::{
    DevIdOption, ExtendedAttribute, File, Metadata, Node, NodeType, SaveOptions, TimeOption,
    XattrOption,
};

use std::path::PathBuf;

fn strip_roots(path: &Path, roots: &Vec<PathBuf>) -> PathBuf {
    let ret = {
        for root in roots {
            if let Ok(stripped) = path.strip_prefix(root) {
                return PathBuf::from("/").join(stripped);
            }
        }

        path.to_path_buf()
    };
    Path::new(&ret.to_string_lossy().replace("\\", "/")).to_path_buf()
}

pub(crate) fn convert_meta(path: &Path, m: &std::fs::Metadata) -> rustic_core::Metadata {
    let (uid, user, gid, group) = utils::user_group(m);
    let (mode, inode, links) = utils::nix_infos(m);
    let extended_attributes = utils::xattrs(path).unwrap_or_default();
    let size = if m.is_dir() { 0 } else { m.len() };
    let device_id = utils::device_id(m);
    rustic_core::Metadata {
        mode,
        mtime: m.modified().ok().and_then(|x| Timestamp::try_from(x).ok()),
        atime: m.accessed().ok().and_then(|x| Timestamp::try_from(x).ok()),
        ctime: m.created().ok().and_then(|x| Timestamp::try_from(x).ok()),
        uid,
        gid,
        user,
        group,
        inode,
        device_id,
        size,
        links,
        extended_attributes,
    }
}

/// [`IgnoreErrorKind`] describes the errors that can be returned by a Ignore action in Backends
#[derive(thiserror::Error, Debug, displaydoc::Display)]
pub enum IgnoreErrorKind {
    #[cfg(all(not(windows), not(target_os = "openbsd")))]
    /// Error getting xattrs for `{path:?}`: `{source:?}`
    ErrorXattr {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Error reading link target for `{path:?}`: `{source:?}`
    ErrorLink {
        path: PathBuf,
        source: std::io::Error,
    },
    #[cfg(not(windows))]
    /// Error converting ctime `{ctime}` and `ctime_nsec` `{ctime_nsec}` to Utc Timestamp: `{source:?}`
    CtimeConversionToTimestampFailed {
        ctime: i64,
        ctime_nsec: i64,
        source: TryFromIntError,
    },
    /// Error acquiring metadata for `{name}`: `{source:?}`
    AcquiringMetadataFailed { name: String, source: ignore::Error },
    /// time error
    JiffError(#[from] jiff::Error),
}

pub type IgnoreResult<T> = Result<T, IgnoreErrorKind>;

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

// ---------------------------------------------------------------------
// Platform-specific helpers
// ---------------------------------------------------------------------

#[cfg(not(windows))]
mod utils {
    use super::{
        BlockdevOption, LocalDestinationErrorKind, LocalDestinationResult, NodeType, nix_mapper,
    };
    use cached::proc_macro::cached;
    use ignore::WalkState;
    use jiff::Timestamp;
    use log::warn;
    use nix::{
        fcntl::{AT_FDCWD, AtFlags},
        sys::stat::{Mode, SFlag, mknod},
        unistd::{Gid, Group, Uid, User, fchownat},
    };
    use rustic_core::{ExtendedAttribute, Metadata, Node};
    use std::{
        ffi::OsStr,
        fs,
        os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
        path::Path,
    };

    /// Cache mapping user name -> uid, since resolving names is comparatively slow.
    #[cached]
    pub fn uid_from_name(name: String) -> Option<Uid> {
        User::from_name(&name)
            .inspect_err(|err| warn!("Cannot determine UID from name {name}: {err}. Using UID 0."))
            .unwrap_or_default()
            .map(|u| u.uid)
    }

    /// Cache mapping group name -> gid.
    #[cached]
    pub fn gid_from_name(name: String) -> Option<Gid> {
        Group::from_name(&name)
            .inspect_err(|err| warn!("Cannot determine GID from name {name}: {err}. Using UID 0."))
            .unwrap_or_default()
            .map(|g| g.gid)
    }

    pub fn ctime(m: &std::fs::Metadata) -> Option<Timestamp> {
        #[allow(clippy::cast_possible_truncation)]
        Timestamp::new(m.ctime(), m.ctime_nsec() as i32).ok()
    }

    pub fn device_id(m: &std::fs::Metadata) -> u64 {
        m.dev()
    }

    pub fn hardlink(m: &std::fs::Metadata) -> bool {
        m.nlink() > 1 && !m.is_dir()
    }

    pub fn user_group(
        m: &std::fs::Metadata,
    ) -> (Option<u32>, Option<String>, Option<u32>, Option<String>) {
        let uid = m.uid();
        let gid = m.gid();
        let user = nix_mapper::get_user_by_uid(uid);
        let group = nix_mapper::get_group_by_gid(gid);
        (Some(uid), user, Some(gid), group)
    }

    pub fn nix_infos(m: &std::fs::Metadata) -> (Option<u32>, u64, u64) {
        let mode = nix_mapper::map_mode_to_go(m.mode());
        let inode = m.ino();
        let links = if m.is_dir() { 0 } else { m.nlink() };
        (Some(mode), inode, links)
    }

    /// List [`ExtendedAttribute`]s for the node located at `path`.
    #[cfg(not(target_os = "openbsd"))]
    pub fn xattrs(path: &Path) -> super::IgnoreResult<Vec<ExtendedAttribute>> {
        xattr::list(path)
            .map_err(|err| super::IgnoreErrorKind::ErrorXattr {
                path: path.to_path_buf(),
                source: err,
            })?
            .map(|name| {
                Ok(ExtendedAttribute {
                    name: name.to_string_lossy().to_string(),
                    value: xattr::get(path, &name).map_err(|err| {
                        super::IgnoreErrorKind::ErrorXattr {
                            path: path.to_path_buf(),
                            source: err,
                        }
                    })?,
                })
            })
            .collect::<super::IgnoreResult<Vec<ExtendedAttribute>>>()
    }

    #[cfg(target_os = "openbsd")]
    pub fn xattrs(_path: &Path) -> super::IgnoreResult<Vec<ExtendedAttribute>> {
        Ok(Vec::new())
    }

    /// Build the [`NodeType`] for a non-symlink entry (device, fifo, socket, directory, or regular file)
    pub fn parse_file_type(m: &std::fs::Metadata) -> NodeType {
        let filetype = m.file_type();
        if filetype.is_block_device() {
            NodeType::Dev { device: m.rdev() }
        } else if filetype.is_char_device() {
            NodeType::Chardev { device: m.rdev() }
        } else if filetype.is_fifo() {
            NodeType::Fifo
        } else if filetype.is_socket() {
            NodeType::Socket
        } else if filetype.is_dir() {
            NodeType::Dir
        } else {
            NodeType::File
        }
    }

    /// Set access/modified times on `filename`.
    pub fn set_times(filename: &Path, meta: &Metadata) -> LocalDestinationResult<()> {
        if let Some(mtime) = meta.mtime {
            let atime = meta.atime.unwrap_or(mtime);
            filetime::set_symlink_file_times(
                filename,
                filetime::FileTime::from_system_time(atime.into()),
                filetime::FileTime::from_system_time(mtime.into()),
            )
            .map_err(LocalDestinationErrorKind::SettingTimeMetadataFailed)?;
        }
        Ok(())
    }

    /// Set user/group on `filename`, resolving `meta.user`/`meta.group` by
    /// name first and falling back to the stored numeric uid/gid.
    pub fn set_user_group(filename: &Path, meta: &Metadata) -> LocalDestinationResult<()> {
        let user = meta.user.clone().and_then(uid_from_name);
        let uid = user.or_else(|| meta.uid.map(Uid::from_raw));

        let group = meta.group.clone().and_then(gid_from_name);
        let gid = group.or_else(|| meta.gid.map(Gid::from_raw));

        fchownat(AT_FDCWD, filename, uid, gid, AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(LocalDestinationErrorKind::FromErrnoError)
    }

    /// Set uid/gid on `filename` from the numeric ids in `meta`.
    pub fn set_uid_gid(filename: &Path, meta: &Metadata) -> LocalDestinationResult<()> {
        let uid = meta.uid.map(Uid::from_raw);
        let gid = meta.gid.map(Gid::from_raw);

        fchownat(AT_FDCWD, filename, uid, gid, AtFlags::AT_SYMLINK_NOFOLLOW)
            .map_err(LocalDestinationErrorKind::FromErrnoError)
    }

    /// Set permissions on `filename` from `node`. No-op for symlinks.
    pub fn set_permission(filename: &Path, node: &Node) -> LocalDestinationResult<()> {
        if node.is_symlink() {
            return Ok(());
        }

        if let Some(mode) = node.meta.mode {
            let mode = nix_mapper::map_mode_from_go(mode);
            fs::set_permissions(filename, fs::Permissions::from_mode(mode))
                .map_err(LocalDestinationErrorKind::SettingFilePermissionsFailed)?;
        }
        Ok(())
    }

    /// Set extended attributes on `filename`, reconciling with what is
    /// currently present (updating changed values, adding missing ones,
    /// removing anything not in `extended_attributes`).
    #[cfg(not(target_os = "openbsd"))]
    pub fn set_extended_attributes(
        filename: &Path,
        extended_attributes: &[ExtendedAttribute],
    ) -> LocalDestinationResult<()> {
        let mut done = vec![false; extended_attributes.len()];
        for curr_name in
            xattr::list(filename).map_err(|err| LocalDestinationErrorKind::ListingXattrsFailed {
                source: err,
                path: filename.to_path_buf(),
            })?
        {
            match extended_attributes.iter().enumerate().find(
                |(_, ExtendedAttribute { name, .. })| name.as_str() == curr_name.to_string_lossy(),
            ) {
                Some((index, ExtendedAttribute { name, value })) => {
                    let curr_value = xattr::get(filename, name).map_err(|err| {
                        LocalDestinationErrorKind::GettingXattrFailed {
                            name: name.clone(),
                            filename: filename.to_path_buf(),
                            source: err,
                        }
                    })?;
                    if value != &curr_value {
                        xattr::set(filename, name, value.as_deref().unwrap_or_default()).map_err(
                            |err| LocalDestinationErrorKind::SettingXattrFailed {
                                name: name.clone(),
                                filename: filename.to_path_buf(),
                                source: err,
                            },
                        )?;
                    }
                    done[index] = true;
                }
                None => {
                    if let Err(err) = xattr::remove(filename, &curr_name) {
                        warn!(
                            "error removing xattr {} on {}: {err}",
                            curr_name.to_string_lossy(),
                            filename.display()
                        );
                    }
                }
            }
        }

        for (index, ExtendedAttribute { name, value }) in extended_attributes.iter().enumerate() {
            if !done[index] {
                xattr::set(filename, name, value.as_deref().unwrap_or_default()).map_err(
                    |err| LocalDestinationErrorKind::SettingXattrFailed {
                        name: name.clone(),
                        filename: filename.to_path_buf(),
                        source: err,
                    },
                )?;
            }
        }

        Ok(())
    }

    #[cfg(target_os = "openbsd")]
    pub fn set_extended_attributes(
        _filename: &Path,
        _extended_attributes: &[ExtendedAttribute],
    ) -> LocalDestinationResult<()> {
        Ok(())
    }

    /// Create a special file (symlink, device, fifo, socket) at `filename`
    /// according to `node`'s type. Regular files/dirs are a no-op here.
    pub fn create_special(filename: &Path, node: &Node) -> LocalDestinationResult<()> {
        match &node.node_type {
            NodeType::Symlink { .. } => {
                let linktarget = node.node_type.to_link();
                symlink(linktarget, filename).map_err(|err| {
                    LocalDestinationErrorKind::SymlinkingFailed {
                        linktarget: linktarget.to_path_buf(),
                        filename: filename.to_path_buf(),
                        source: err,
                    }
                })?;
            }
            NodeType::Dev { device } => {
                let device = convert_device_id(*device)?;
                mknod(filename, SFlag::S_IFBLK, Mode::empty(), device)
                    .map_err(LocalDestinationErrorKind::FromErrnoError)?;
            }
            NodeType::Chardev { device } => {
                let device = convert_device_id(*device)?;
                mknod(filename, SFlag::S_IFCHR, Mode::empty(), device)
                    .map_err(LocalDestinationErrorKind::FromErrnoError)?;
            }
            NodeType::Fifo => {
                mknod(filename, SFlag::S_IFIFO, Mode::empty(), 0)
                    .map_err(LocalDestinationErrorKind::FromErrnoError)?;
            }
            NodeType::Socket => {
                mknod(filename, SFlag::S_IFSOCK, Mode::empty(), 0)
                    .map_err(LocalDestinationErrorKind::FromErrnoError)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Convert a stored `u64` device id to whatever type `mknod` expects
    /// on this target (varies by OS/arch).
    #[cfg(not(any(
        target_os = "macos",
        target_os = "openbsd",
        all(target_os = "android", target_pointer_width = "32")
    )))]
    fn convert_device_id(device: u64) -> LocalDestinationResult<u64> {
        Ok(device)
    }

    #[cfg(any(target_os = "macos", target_os = "openbsd"))]
    fn convert_device_id(device: u64) -> LocalDestinationResult<i32> {
        i32::try_from(device).map_err(|err| LocalDestinationErrorKind::DeviceIdConversionFailed {
            target: "i32".to_string(),
            device,
            source: err,
        })
    }

    #[cfg(all(target_os = "android", target_pointer_width = "32"))]
    fn convert_device_id(device: u64) -> LocalDestinationResult<u32> {
        u32::try_from(device).map_err(|err| LocalDestinationErrorKind::DeviceIdConversionFailed {
            target: "u32".to_string(),
            device,
            source: err,
        })
    }
}

#[cfg(windows)]
mod utils {
    use super::{BlockdevOption, LocalDestinationResult, NodeType};
    use jiff::Timestamp;
    use rustic_core::{ExtendedAttribute, Metadata, Node};
    use std::{ffi::OsStr, path::Path};

    /// Build the [`NodeType`] for a non-symlink entry (device, fifo, socket, directory, or regular file)
    pub fn parse_file_type(m: &std::fs::Metadata) -> NodeType {
        let filetype = m.file_type();
        if filetype.is_dir() {
            NodeType::Dir
        } else {
            NodeType::File
        }
    }

    /// Windows has no unix ctime; fall back to file creation time.
    pub fn ctime(m: &std::fs::Metadata) -> Option<Timestamp> {
        m.created().ok().and_then(|t| Timestamp::try_from(t).ok())
    }

    /// Device ids are not meaningful on Windows.
    pub fn device_id(_m: &std::fs::Metadata) -> u64 {
        0
    }

    /// Hardlink detection is not implemented on Windows.
    pub fn hardlink(_m: &std::fs::Metadata) -> bool {
        false
    }

    /// Windows has no unix uid/gid/user/group model.
    pub fn user_group(
        _m: &std::fs::Metadata,
    ) -> (Option<u32>, Option<String>, Option<u32>, Option<String>) {
        (None, None, None, None)
    }

    /// No unix mode/inode/link-count on Windows.
    pub fn nix_infos(_m: &std::fs::Metadata) -> (Option<u32>, u64, u64) {
        (None, 0, 0)
    }

    /// Extended attributes are not read on Windows.
    pub fn xattrs(_path: &Path) -> super::IgnoreResult<Vec<ExtendedAttribute>> {
        Ok(Vec::new())
    }

    /// Windows has no block/char devices, fifos, or sockets to distinguish
    /// here; everything that isn't a dir/symlink is a plain file.
    pub fn to_node_other(
        _blockdev: BlockdevOption,
        name: &OsStr,
        _m: &std::fs::Metadata,
        meta: Metadata,
    ) -> Node {
        Node::new_node(name, NodeType::File, meta)
    }

    // -- Restoration (destination side) ----------------------------------

    /// TODO: Windows support. Setting file times on Windows isn't wired up
    /// yet; currently a no-op.
    pub fn set_times(_filename: &Path, _meta: &Metadata) -> LocalDestinationResult<()> {
        Ok(())
    }

    /// TODO: Windows support.
    /// See https://learn.microsoft.com/windows/win32/fileio/file-security-and-access-rights
    pub fn set_user_group(_filename: &Path, _meta: &Metadata) -> LocalDestinationResult<()> {
        Ok(())
    }

    /// TODO: Windows support.
    pub fn set_uid_gid(_filename: &Path, _meta: &Metadata) -> LocalDestinationResult<()> {
        Ok(())
    }

    /// TODO: Windows support.
    pub fn set_permission(_filename: &Path, _node: &Node) -> LocalDestinationResult<()> {
        Ok(())
    }

    /// TODO: Windows support.
    pub fn set_extended_attributes(
        _filename: &Path,
        _extended_attributes: &[ExtendedAttribute],
    ) -> LocalDestinationResult<()> {
        Ok(())
    }

    /// TODO: Windows support. Symlinks/devices/fifos/sockets aren't
    /// created on restore yet; currently a no-op.
    pub fn create_special(_filename: &Path, _node: &Node) -> LocalDestinationResult<()> {
        Ok(())
    }
}

pub(crate) use utils::*;
