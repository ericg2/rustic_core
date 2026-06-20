use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
};
use std::io::Write;
use derive_setters::Setters;
use ignore::{Walk, WalkBuilder};
use log::warn;
use serde_with::serde_as;

use rustic_core::{ErrorKind, Excludes, FilterOptions, PathList, ReadFileOpen, ReadSource, ReadSourceBuilder, ReadSourceEntry, RusticError, RusticResult, WriteFileOpen};

use crate::local::mapper::LocalSaveOptions;
use serde::{Deserialize, Serialize};
#[cfg(not(windows))]
use std::num::TryFromIntError;

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

/// A [`LocalReader`] is a source from local paths which is used to be read from (i.e. to backup it).
#[derive(Debug)]
pub struct LocalReader {
    /// The walk builder.
    builder: WalkBuilder,
    /// The local source to use.
    src: LocalSource,
}

impl LocalReader {
    /// Create a local source from [`LocalSaveOptions`], [`LocalSourceFilterOptions`] and backup path(s).
    ///
    /// # Arguments
    ///
    /// * `filter_opts` - The [`LocalSourceFilterOptions`] to use.
    /// * `backup_paths` - The backup path(s) to use.
    ///
    /// # Returns
    ///
    /// The created local source.
    ///
    /// # Errors
    ///
    /// * If the a glob pattern could not be added to the override builder.
    /// * If a glob file could not be read.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn new(src: &LocalSource) -> RusticResult<Self> {
        let mut builder = WalkBuilder::new(&src.paths[0]);
        for path in &src.paths[1..] {
            _ = builder.add(path);
        }

        let overrides = src.excludes.clone().unwrap_or_default().as_override()?;
        let filter_opts = src.filter_opts.clone().unwrap_or_default();
        for file in &filter_opts.custom_ignorefiles {
            _ = builder.add_custom_ignore_filename(file);
        }

        _ = builder
            .follow_links(false)
            .hidden(false)
            .ignore(false)
            .git_ignore(filter_opts.git_ignore)
            .git_exclude(filter_opts.git_ignore)
            .require_git(!filter_opts.no_require_git)
            .sort_by_file_path(Path::cmp)
            .same_file_system(filter_opts.one_file_system)
            .max_filesize(filter_opts.exclude_larger_than.map(|s| s.as_u64()))
            .overrides(overrides);

        let exclude_if_present = filter_opts.exclude_if_present.clone();
        let exclude_if_xattr: Vec<OsString> = filter_opts
            .exclude_if_xattr
            .iter()
            .map(OsString::from)
            .collect();

        if !exclude_if_xattr.is_empty() {
            #[cfg(any(windows, target_os = "openbsd"))]
            warn!("exclude-if-xattr is not supported on this platform");
            #[cfg(not(any(windows, target_os = "openbsd")))]
            if !xattr::SUPPORTED_PLATFORM {
                warn!("exclude-if-xattr is not supported on this platform");
            }
        }

        if !exclude_if_present.is_empty() || !exclude_if_xattr.is_empty() {
            _ = builder.filter_entry(move |entry| {
                // exclude-if-present: skip directories containing a marker file
                if !exclude_if_present.is_empty()
                    && let Some(tpe) = entry.file_type()
                    && tpe.is_dir()
                    && exclude_if_present
                        .iter()
                        .any(|file| entry.path().join(file).exists())
                {
                    return false;
                }

                // exclude-if-xattr: skip entries that have a matching xattr
                #[cfg(not(any(windows, target_os = "openbsd")))]
                if xattr::SUPPORTED_PLATFORM && !exclude_if_xattr.is_empty() {
                    match xattr::list(entry.path()) {
                        Ok(mut attrs) => {
                            if attrs.any(|attr| exclude_if_xattr.contains(&attr)) {
                                return false;
                            }
                        }
                        Err(err) => {
                            warn!(
                                "Error reading xattrs for {}, not excluding: {err}",
                                entry.path().display()
                            );
                        }
                    }
                }

                true
            });
        }

        Ok(Self {
            builder,
            src: src.to_owned(),
        })
    }
}

// Walk doesn't implement Debug
#[allow(missing_debug_implementations)]
pub struct LocalIterator {
    /// The walk iterator.
    walker: Walk,
    /// The config to use.
    save_opts: LocalSaveOptions,
    /// The roots that have been used.
    roots: Vec<PathBuf>,
}

impl Iterator for LocalIterator {
    type Item = RusticResult<ReadSourceEntry<LocalFile>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.walker.next() {
            // ignore root dir, i.e. an entry with depth 0 of type dir
            Some(Ok(entry)) if entry.depth() == 0 && entry.file_type().unwrap().is_dir() => {
                self.walker.next()
            }
            item => item,
        }
        .map(|e| {
            self.save_opts
                .map_entry(
                    &self.roots,
                    e.map_err(|err| {
                        RusticError::with_source(
                            ErrorKind::Internal,
                            "Failed to get next entry from walk iterator.",
                            err,
                        )
                        .ask_report()
                    })?,
                )
                .map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::Internal,
                        "Failed to map Directory entry to ReadSourceEntry.",
                        err,
                    )
                    .ask_report()
                })
        })
    }
}

#[derive(Debug)]
/// Describes an open file from the local backend.
pub struct LocalFile(pub(crate) PathBuf);

impl ReadFileOpen for LocalFile {
    type Reader = File;

    fn open(self) -> RusticResult<Self::Reader> {
        let path = self.0;
        File::open(&path).map_err(|err| {
            RusticError::with_source(
                ErrorKind::InputOutput,
                "Failed to open file at `{path}`. Please make sure the file exists and is accessible.",
                err,
            )
            .attach_context("path", path.display().to_string())
        })
    }
}


impl WriteFileOpen for LocalFile {
    type Writer = File;

    fn open_replace(self) -> RusticResult<Self::Writer> {
        let path = self.0;
        File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::InputOutput,
                    "Failed to open file for writing at `{path}`.",
                    err,
                )
                    .attach_context("path", path.display().to_string())
            })
    }
}

impl ReadSource for LocalReader {
    type Open = LocalFile;
    type Iter = LocalIterator;

    fn size(&self) -> RusticResult<Option<u64>> {
        let mut size = 0;
        for entry in self.builder.build() {
            if let Err(err) = entry.and_then(|e| e.metadata()).map(|m| {
                size += if m.is_dir() { 0 } else { m.len() };
            }) {
                warn!("ignoring error {err}");
            }
        }
        Ok(Some(size))
    }

    fn entries(&self) -> Self::Iter {
        LocalIterator {
            walker: self.builder.build(),
            save_opts: self.src.save_opts.unwrap_or_default(),
            roots: self.src.paths.clone(),
        }
    }

    fn paths(&self) -> Vec<PathBuf> {
        self.src.paths.clone()
    }

    fn close(self) -> RusticResult<()> {
        Ok(())
    }
}

#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[setters(into)]
#[non_exhaustive]
/// A source that contains local files.
pub struct LocalSource {
    pub paths: Vec<PathBuf>,
    pub excludes: Option<Excludes>,
    pub filter_opts: Option<FilterOptions>,
    pub save_opts: Option<LocalSaveOptions>,
}

impl LocalSource {
    /// Creates a new [`LocalSource`] with the given paths.
    pub fn new(paths: impl Into<PathList>) -> Self {
        Self {
            paths: paths.into().paths(),
            excludes: None,
            filter_opts: None,
            save_opts: None,
        }
    }
}

impl ReadSourceBuilder for LocalSource {
    type Reader = LocalReader;

    fn get_reader(&self) -> RusticResult<Self::Reader> {
        if self.paths.is_empty() {
            return Err(RusticError::new(
                ErrorKind::Configuration,
                "One or more paths are required for source",
            ));
        }

        LocalReader::new(&self)
    }
}
