use crate::opendal::opendal_src::OpenDALReader;
use crate::opendal::{OpenDALBackend, OpenDALSource, OpenDALConfig};
use bytes::Bytes;
use opendal::options::{DeleteOptions, ReadOptions, WriteOptions};
use opendal::raw::BytesRange;
use rustic_core::{Destination, ErrorKind, Metadata, Node, ReadSourceBuilder, RestoreOptions, RusticError, RusticResult, DestinationBuilder};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use derive_setters::Setters;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use crate::path_to_str;

/// OpenDAL destination, used when restoring.
#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Debug, Setters, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct OpenDALDestination {
    #[setters(skip)]
    root: PathBuf,

    #[setters(skip)]
    config: OpenDALConfig,
}

impl OpenDALDestination {
    /// Create a new [`OpenDALDestination`].
    ///
    /// # Arguments
    ///
    /// * `config` - The [`OpenDALConfig`] to use.
    /// * `root` - The base path of the destination
    pub fn new(config: &OpenDALConfig, root: impl AsRef<Path>) -> Self {
        Self {
            config: config.to_owned(),
            root: root.as_ref().to_path_buf(),
        }
    }
}

impl DestinationBuilder for OpenDALDestination {
    type Output = OpenDALWriter;

    fn get_destination(&self) -> RusticResult<Self::Output> {
        let be = OpenDALBackend::new(&self.config)?;
        OpenDALWriter::new(Arc::new(be), &self)
    }
}

#[derive(Clone, Debug)]
pub struct OpenDALWriter {
    dest: OpenDALDestination,
    be: Arc<OpenDALBackend>,
}

impl OpenDALWriter {
    pub(crate) fn new(be: Arc<OpenDALBackend>, dest: &OpenDALDestination) -> RusticResult<Self> {
        Ok(Self {
            be,
            dest: dest.to_owned(),
        })
    }
}

impl Destination for OpenDALWriter {
    type Reader = OpenDALReader;

    fn path(&self, path: &Path) -> PathBuf {
        crate::join_force(&self.dest.root, path)
    }

    fn read_source(&self) -> RusticResult<Self::Reader> {
        OpenDALSource::new(&self.dest.config, &self.dest.root).get_reader()
    }

    fn remove_dir(&self, path: &Path) -> RusticResult<()> {
        let path = path_to_str(&self.dest.root, path, true);
        self.be
            .operator
            .delete_options(
                &path,
                DeleteOptions {
                    recursive: true,
                    ..Default::default()
                },
            )
            .map_err(|err| {
                RusticError::with_source(ErrorKind::Backend, "Failed to remove directory", err)
            })?;
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> RusticResult<()> {
        let path = path_to_str(&self.dest.root, path, false);
        self.be.operator.delete(&path).map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Failed to remove file", err)
        })?;
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> RusticResult<()> {
        let path = path_to_str(&self.dest.root, path, true);
        self.be.operator.create_dir(&path).map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Failed to read directory", err)
        })?;
        Ok(())
    }

    fn set_restore_metadata(
        &self,
        _path: &Path,
        _node: &Node,
        _opts: &RestoreOptions,
    ) -> RusticResult<()> {
        Ok(())
    }

    fn set_length(&self, path: &Path, size: u64) -> RusticResult<()> {
        let path = path_to_str(&self.dest.root, path, false);

        if size == 0 {
            self.be
                .operator
                .write(&path, Vec::<u8>::new())
                .map_err(|err| {
                    RusticError::with_source(ErrorKind::Backend, "Failed to set length", err)
                })?;
            return Ok(());
        }

        // OpenDAL doesn't provide a generic truncate API.
        // Create a placeholder object of the requested size.
        // self.be.operator.write(&path, vec![0u8; size as usize])?;
        Ok(())
    }

    fn read_exact(&self, path: &Path, offset: u64, length: u64) -> RusticResult<Bytes> {
        let path = path_to_str(&self.dest.root, path, false);
        let mut buf = vec![0; length as usize];
        self.be
            .operator
            .read_options(
                &path,
                ReadOptions {
                    range: BytesRange::from(offset..offset + length),
                    ..Default::default()
                },
            )
            .map_err(|err| {
                RusticError::with_source(ErrorKind::Backend, "Failed to read metadata", err)
            })?
            .read_exact(&mut buf)
            .map_err(|err| {
                RusticError::with_source(ErrorKind::Backend, "Failed to read file", err)
            })?;

        Ok(Bytes::from(buf))
    }

    fn get_existing(&self, path: &Path) -> RusticResult<Option<Metadata>> {
        let path = path_to_str(&self.dest.root, path, false);
        let meta = match self.be.operator.stat(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == opendal::ErrorKind::NotFound => return Ok(None),
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
            mtime: meta.last_modified().map(|x| x.into_inner()),
            atime: None,
            ctime: None,
            uid: None,
            gid: None,
            user: None,
            group: None,
            inode: 0,
            device_id: 0,
            size: meta.content_length(),
            links: 0,
            extended_attributes: vec![],
        };

        Ok(Some(ret))
    }

    fn write_at(&self, _path: &Path, _offset: u64, _data: &[u8]) -> RusticResult<()> {
        unreachable!("write_at should never be called when can_random_write() is false")
    }

    fn hard_link(&self, _path: &Path, _item: &Path) -> RusticResult<()> {
        unreachable!("hard_link should never be called when can_hard_link() is false")
    }

    fn append(&self, path: &Path, data: &[u8]) -> RusticResult<()> {
        let path = path_to_str(&self.dest.root, path, false);
        self.be
            .operator
            .writer_options(
                &path,
                WriteOptions {
                    append: true,
                    ..Default::default()
                },
            )
            .map_err(|err| {
                RusticError::with_source(ErrorKind::Backend, "Failed to read metadata", err)
            })?
            .write(data.to_vec())
            .map_err(|err| {
                RusticError::with_source(ErrorKind::Backend, "Failed to read metadata", err)
            })?;

        Ok(())
    }

    fn can_random_write(&self) -> bool {
        false
    }

    fn can_hard_link(&self) -> bool {
        false
    }
}
