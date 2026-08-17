use derive_setters::Setters;
use ignore::{Walk, WalkBuilder};
use log::warn;
use serde_with::serde_as;
use std::io::{self, Write};
use std::sync::Arc;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use rustic_core::{
    ErrorKind, Excludes, File, FileLister, FilterOptions, PathList, ReadFileOpen, ReadSourceConfig,
    RusticError, RusticResult, WriteFileOpen,
};

use crate::local::backend::LocalSource;
use crate::local::mapper::{self, LocalSaveOptions};
use serde::{Deserialize, Serialize};

#[cfg(not(windows))]
use std::num::TryFromIntError;

/// A [`LocalWalker`] is a source from local paths which is used to be read from (i.e. to backup it).
// Walk doesn't implement Debug
#[allow(missing_debug_implementations)]
pub(crate) struct LocalWalker(Walk, Arc<LocalSource>);

impl LocalWalker {
    #[allow(clippy::too_many_lines)]
    pub fn new(be: LocalSource, root: &Path, opts: ListOptions) -> io::Result<Self> {
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

        Ok(Self(builder.build(), Arc::new(be)))
    }
}

impl Iterator for LocalWalker {
    type Item = io::Result<File>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next() {
            // ignore root dir, i.e. an entry with depth 0 of type dir
            Some(Ok(entry)) if entry.depth() == 0 && entry.file_type().unwrap().is_dir() => {
                self.walker.next()
            }
            item => item,
        }
        .map(|e| {
            let e = e?;
            let path = e.path()?;
            let m = e.metadata()?;
            let meta = mapper::convert_meta(path, &m);
            let node_type = mapper::parse_file_type(&m);
            File::new(
                path.to_path_buf(),
                node_type,
                meta,
                Some(self.1.clone()),
                Some(self.1.clone()),
            )
        })
    }
}
