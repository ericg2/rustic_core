use crate::opendal::source::{OpenDALFile, OpenDALReader};
use crate::opendal::{OpenDALBackend, OpenDALConfig, OpenDALSource};
use crate::path_to_str;
use derive_setters::Setters;
use opendal::options::{DeleteOptions, WriteOptions};
use rustic_core::{
    Destination, DestinationBuilder, ErrorKind, Metadata, Node, ReadSourceBuilder, RestoreOptions,
    RusticError, RusticResult,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// OpenDAL destination, used when restoring.
#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct OpenDALDestination {
    /// The path to use for destination.
    pub path: Option<PathBuf>,

    /// The [`OpenDALConfig`] to use.
    pub config: Option<OpenDALConfig>,
}

impl OpenDALDestination {
    /// Create a new [`OpenDALDestination`].
    ///
    /// # Arguments
    ///
    /// * `path` - The base path of the destination
    /// * `config` - The [`OpenDALRepo`] to use.
    pub fn new(path: impl AsRef<Path>, config: &OpenDALConfig) -> Self {
        Self {
            path: Some(path.as_ref().to_path_buf()),
            config: Some(config.to_owned()),
        }
    }
}

impl DestinationBuilder for OpenDALDestination {
    type Output = OpenDALWriter;

    fn get_destination(&self) -> RusticResult<Self::Output> {
        // Make sure the fields are valid and filled in.
        let root = self.path.as_ref().ok_or(RusticError::new(
            ErrorKind::Configuration,
            "Root is required for source.",
        ))?;
        let config = self.config.as_ref().ok_or(RusticError::new(
            ErrorKind::Configuration,
            "OpenDAL Config is required for source.",
        ))?;

        let be = OpenDALBackend::new(&config)?;
        let ret = OpenDALWriter::new(Arc::new(be), root.clone(), config.clone());
        Ok(ret)
    }
}

#[derive(Clone, Debug)]
pub struct OpenDALWriter {
    be: Arc<OpenDALBackend>,
    root: PathBuf,
    config: OpenDALConfig,
}

impl OpenDALWriter {
    pub(crate) fn new(be: Arc<OpenDALBackend>, root: PathBuf, config: OpenDALConfig) -> Self {
        Self { be, root, config }
    }
}

impl Destination for OpenDALWriter {
    type Iterator = OpenDALReader;
    type Reader = OpenDALFile;
    type Writer = OpenDALFile;

    fn read_source(&self) -> RusticResult<Self::Iterator> {
        OpenDALSource::new(&self.config, &self.root).get_reader()
    }

    fn remove_dir(&self, path: &Path) -> RusticResult<()> {
        let path = path_to_str(&self.root, path, true);
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
        let path = path_to_str(&self.root, path, false);
        self.be.operator.delete(&path).map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Failed to remove file", err)
        })?;
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> RusticResult<()> {
        let path = path_to_str(&self.root, path, true);
        if path != "/" {
            // OpenDAL does not allow creating a root directory. Don't do this on restore!
            self.be.operator.create_dir(&path).map_err(|err| {
                RusticError::with_source(ErrorKind::Backend, "Failed to read directory", err)
            })?;
        }
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
        let path = path_to_str(&self.root, path, false);

        if size == 0 {
            let _ = self
                .be
                .operator
                .write(&path, Vec::<u8>::new())
                .map_err(|err| {
                    RusticError::with_source(ErrorKind::Backend, "Failed to set length", err)
                })?;
            return Ok(());
        }

        // OpenDAL doesn't provide a generic truncate API.
        Err(RusticError::new(
            ErrorKind::Unsupported,
            "Cannot set OpenDAL length > 0",
        ))
    }

    fn get_reader(&self, path: &Path) -> RusticResult<Self::Reader> {
        Ok(OpenDALFile(
            self.be.clone(),
            path_to_str(&self.root, path, false),
        ))
    }

    fn get_writer(&self, path: &Path) -> RusticResult<Self::Writer> {
        Ok(OpenDALFile(
            self.be.clone(),
            path_to_str(&self.root, path, false),
        ))
    }

    fn get_existing(&self, path: &Path) -> RusticResult<Option<Metadata>> {
        let path = path_to_str(&self.root, path, false);
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
        let path = path_to_str(&self.root, path, false);
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
