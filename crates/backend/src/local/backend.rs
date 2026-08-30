use aho_corasick::AhoCorasick;
use bytes::Bytes;
use derive_setters::Setters;
use ignore::DirEntry;
use log::{debug, error, trace, warn};
use std::fs::File;
use std::{
    fmt::Debug,
    fs::{self, Metadata, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

use crate::local::{config::LocalConfig, mapper};
use rustic_core::{
    ALL_FILE_TYPES, CommandInput, ErrorKind, FileType, Id, Node, ReadBackend, ReadHandle,
    ReadSource, RusticError, RusticResult, WriteBackend, WriteSource,
};

/// A local backend.
#[derive(Clone, Debug)]
pub struct LocalSource(PathBuf);

impl LocalSource {
    pub fn new(path: impl AsRef<Path>) -> Self{
        Self(path.as_ref().to_path_buf())
    }

    pub fn from_config(config: &LocalConfig) -> RusticResult<Self> {
        let path = config.path.clone().ok_or_else(|| {
            RusticError::new(ErrorKind::InvalidInput, "Path is required for Local Config")
        })?;
        Ok(Self(path))
    }

    fn fix_path(&self, path: impl AsRef<Path>) -> PathBuf {
        crate::join_force(&self.0, path.as_ref())
    }
}

impl ReadSource for LocalSource {
    fn location(&self) -> String {
        self.0.to_string_lossy().to_string()
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadHandle>> {
        let path = self.fix_path(path);
        let file = File::open(&path)?;
        Ok(Box::new(file))
    }

    fn readdir(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<Node>> + Send>> {
        let entries = fs::read_dir(path)?.map(|entry| {
            let entry = entry?;
            let metadata = entry.metadata()?;
            let converted_meta = mapper::convert_meta(&entry.path(), &metadata);
            let file_type = mapper::parse_file_type(&metadata);
            Ok(Node::new_node(
                &entry.file_name(),
                file_type,
                converted_meta,
            ))
        });
        Ok(Box::new(entries))
    }

    fn stat(&self, path: &Path) -> std::io::Result<Option<rustic_core::Metadata>> {
        let path = self.fix_path(path);
        match fs::symlink_metadata(&path) {
            Ok(meta) => Ok(Some(mapper::convert_meta(&path, &meta))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        }
    }
}

impl WriteSource for LocalSource {
    fn remove_dir(&self, path: &Path) -> std::io::Result<()> {
        let path = self.fix_path(path);
        fs::remove_dir_all(&path)?;
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        let path = self.fix_path(path);
        fs::remove_file(&path)?;
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        let path = self.fix_path(path);
        fs::create_dir_all(&path)?;
        Ok(())
    }

    fn set_restore_metadata(
        &self,
        path: &Path,
        node: &rustic_core::Node,
        opts: &rustic_core::RestoreOptions,
    ) -> std::io::Result<()> {
        mapper::create_special(path, node)
            .unwrap_or_else(|_| warn!("restore {}: creating special file failed.", path.display()));
        match (opts.no_ownership, opts.numeric_id) {
            (true, _) => {}
            (false, true) => mapper::set_uid_gid(path, &node.meta)
                .unwrap_or_else(|_| warn!("restore {}: setting UID/GID failed.", path.display())),
            (false, false) => mapper::set_user_group(path, &node.meta).unwrap_or_else(|_| {
                warn!("restore {}: setting User/Group failed.", path.display())
            }),
        }
        mapper::set_permission(path, node)
            .unwrap_or_else(|_| warn!("restore {}: chmod failed.", path.display()));
        mapper::set_extended_attributes(path, &node.meta.extended_attributes).unwrap_or_else(
            |_| {
                warn!(
                    "restore {}: setting extended attributes failed.",
                    path.display()
                );
            },
        );
        mapper::set_times(path, &node.meta)
            .unwrap_or_else(|_| warn!("restore {}: setting file times failed.", path.display()));
        Ok(())
    }

    fn set_length(&self, path: &Path, size: u64) -> std::io::Result<()> {
        let path = self.fix_path(path);
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?
            .set_len(size)?;
        Ok(())
    }

    fn open_replace(&self, path: &Path) -> std::io::Result<Box<dyn rustic_core::WriteHandle>> {
        let path = self.fix_path(path);
        let ret = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Box::new(ret))
    }

    fn write_all(&self, path: &Path, bytes: Bytes) -> std::io::Result<()> {
        fn write_local_file(filename: &Path, buf: &[u8]) -> io::Result<()> {
            let length = buf
                .len()
                .try_into()
                .map_err(|err| std::io::Error::new(io::ErrorKind::InvalidInput, err))?;

            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(filename)?;

            file.set_len(length)?;
            file.write_all(buf)?;
            file.sync_all()?;
            Ok(())
        }

        let filename = self.fix_path(path);
        let parent = filename.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "path has no parent directory",
            )
        })?;

        fs::create_dir_all(parent)?;

        let filename_tmp = parent.join(
            filename
                .file_name()
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
                })?
                .to_string_lossy()
                .to_string()
                + "-tmp-",
        );

        match write_local_file(&filename_tmp, &bytes) {
            Ok(file) => file,
            Err(err) => {
                _ = std::fs::remove_file(&filename_tmp);
                return Err(err);
            }
        }

        fs::rename(&filename_tmp, &filename)?;
        Ok(())
    }

    fn write_at(&self, path: &Path, offset: u64, data: &[u8]) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;

        let _ = file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        Ok(())
    }

    fn hard_link(&self, path: &Path, item: &Path) -> std::io::Result<()> {
        let path = self.fix_path(path);
        let item = self.fix_path(item);
        fs::hard_link(&path, &item)?;
        Ok(())
    }

    fn can_random_write(&self) -> bool {
        true
    }

    fn can_hard_link(&self) -> bool {
        true
    }
}
