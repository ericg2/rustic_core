use crate::filter::ExcludeFilter;
use crate::opendal::{OpenDALBackend, OpenDALConfig, OpenDALDestination};

use log::warn;
use opendal::blocking::StdReader;
use opendal::options::ListOptions;
use opendal::{Builder, Configurator, Entry, IntoOperatorUri};

use rustic_core::{
    ErrorKind, Excludes, Node, NodeType, PathList, ReadFileOpen, ReadSource, ReadSourceBuilder,
    ReadSourceEntry, RusticError, RusticResult,
};

use crate::local::LocalSource;
use derive_setters::Setters;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Setters)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
/// OpenDAL-backed source definition
pub struct OpenDALSource {
    #[setters(skip)]
    paths: Vec<PathBuf>,

    #[setters(skip)]
    config: OpenDALConfig,

    excludes: Option<Excludes>,
}

impl OpenDALSource {
    /// Creates a new [`OpenDALSource`] with the given paths.
    pub fn new(config: &OpenDALConfig, paths: impl Into<PathList>) -> Self {
        Self {
            paths: paths.into().paths(),
            config: config.to_owned(),
            excludes: None,
        }
    }
}

impl ReadSourceBuilder for OpenDALSource {
    type Reader = OpenDALReader;

    fn get_reader(&self) -> RusticResult<Self::Reader> {
        let be = OpenDALBackend::new(&self.config)?;
        OpenDALReader::new(Arc::new(be), &self)
    }
}

/// Describes an open file from the OpenDAL backend.
#[derive(Debug, Clone)]
pub struct OpenDALFile(Arc<OpenDALBackend>, String);

impl ReadFileOpen for OpenDALFile {
    type Reader = StdReader;

    fn open(self) -> RusticResult<Self::Reader> {
        let path = self.1;
        let reader = self
            .0
            .operator
            .reader(&path)
            .and_then(|r| r.into_std_read(..))
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::InputOutput,
                    "Failed to open file at `{path}`. Please ensure it exists and is accessible.",
                    err,
                )
                .attach_context("path", path.clone())
            })?;

        Ok(reader)
    }
}

pub struct OpenDALIterator {
    entries: std::vec::IntoIter<ReadSourceEntry<OpenDALFile>>,
}

impl Iterator for OpenDALIterator {
    type Item = RusticResult<ReadSourceEntry<OpenDALFile>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(Ok)
    }
}

#[derive(Debug)]
pub struct OpenDALReader {
    entries: Vec<ReadSourceEntry<OpenDALFile>>,
    be: Arc<OpenDALBackend>,
    config: OpenDALSource,
}

impl ReadSource for OpenDALReader {
    type Open = OpenDALFile;
    type Iter = OpenDALIterator;

    fn size(&self) -> RusticResult<Option<u64>> {
        Ok(Some(self.entries.iter().map(|e| e.node.meta.size).sum()))
    }

    fn entries(&self) -> Self::Iter {
        OpenDALIterator {
            entries: self.entries.clone().into_iter(),
        }
    }

    fn paths(&self) -> Vec<PathBuf> {
        self.config.paths.clone()
    }
}

impl OpenDALReader {
    pub(crate) fn new(be: Arc<OpenDALBackend>, config: &OpenDALSource) -> RusticResult<Self> {
        Ok(Self {
            entries: Self::map_all(be.clone(), config)?,
            config: config.to_owned(),
            be,
        })
    }

    fn map_all(
        be: Arc<OpenDALBackend>,
        config: &OpenDALSource,
    ) -> RusticResult<Vec<ReadSourceEntry<OpenDALFile>>> {
        let filter = config
            .excludes
            .clone()
            .map(ExcludeFilter::new)
            .transpose()?;
        let list_options = ListOptions {
            recursive: true,
            ..Default::default()
        };

        let mut all_entries = Vec::new();
        for root in &config.paths {
            let path = crate::path_to_str(root, "", true);
            let lister = be
                .operator
                .lister_options(&path, list_options.clone())
                .map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::Backend,
                        "Error listing OpenDAL source.",
                        err,
                    )
                })?;

            let mut entries: Vec<_> = lister
                .filter_map(|entry| match entry {
                    Ok(e) if e.path() != "/" => Some(e),
                    Ok(_) => None,
                    Err(e) => {
                        warn!("Ignoring OpenDAL entry error: {e}");
                        None
                    }
                })
                .map(|e| Self::map_entry(be.clone(), e))
                .filter(|e| filter.as_ref().is_none_or(|x| x.is_ok(&e)))
                .collect();

            all_entries.append(&mut entries);
        }
        all_entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Ok(all_entries)
    }

    fn map_entry(be: Arc<OpenDALBackend>, e: Entry) -> ReadSourceEntry<OpenDALFile> {
        let path = e.path().strip_suffix('/').unwrap_or(e.path()).to_string();
        let metadata = e.metadata();
        let node_type = if metadata.is_dir() {
            NodeType::Dir
        } else {
            NodeType::File
        };

        let meta = rustic_core::repofile::Metadata {
            mtime: metadata
                .last_modified()
                .map(opendal::raw::Timestamp::into_inner),
            size: metadata.content_length(),
            ..Default::default()
        };

        ReadSourceEntry {
            path: path.clone().into(),
            node: Node::new_node(OsStr::new(e.name()), node_type, meta),
            open: Some(OpenDALFile(be, path)),
        }
    }
}