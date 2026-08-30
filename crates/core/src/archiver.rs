pub(crate) mod file_archiver;
pub(crate) mod parent;
pub(crate) mod tree;
pub(crate) mod tree_archiver;

use std::path::{Path, PathBuf};
use std::thread::scope;

use jiff::Zoned;
use log::warn;
use pariter::IteratorExt;

use crate::{
    CancelToken, ListAdapter, Progress, ReadSource,
    archiver::{
        file_archiver::FileArchiver, parent::Parent, tree::TreeIterator,
        tree_archiver::TreeArchiver,
    },
    backend::{File, FileLister, decrypt::DecryptFullBackend},
    blob::BlobType,
    error::RusticResult,
    index::{
        ReadGlobalIndex,
        indexer::{Indexer, SharedIndexer},
    },
    repofile::{configfile::ConfigFile, snapshotfile::SnapshotFile},
};

#[derive(thiserror::Error, Debug, displaydoc::Display)]
/// Tree stack empty
pub struct TreeStackEmptyError;

/// The `Archiver` is responsible for archiving files and trees.
/// It will read the file, chunk it, and write the chunks to the backend.
///
/// # Type Parameters
///
/// * `BE` - The backend type.
/// * `I` - The index to read from.
#[allow(missing_debug_implementations)]
#[allow(clippy::struct_field_names)]
pub struct Archiver<'a, BE: DecryptFullBackend, I: ReadGlobalIndex, R: ReadSource> {
    /// The `FileArchiver` is responsible for archiving files.
    file_archiver: FileArchiver<'a, BE, I, R>,

    /// The `TreeArchiver` is responsible for archiving trees.
    tree_archiver: TreeArchiver<'a, BE, I>,

    /// The parent snapshot to use.
    parent: Parent,

    /// The `SharedIndexer` is used to index the data.
    indexer: SharedIndexer<BE>,

    /// The backend to write to.
    be: BE,

    /// The backend to write to.
    index: &'a I,

    src: &'a R,

    /// The `SnapshotFile` to write to.
    snap: SnapshotFile,
}

impl<'a, BE: DecryptFullBackend, I: ReadGlobalIndex, R: ReadSource> Archiver<'a, BE, I, R> {
    /// Creates a new `Archiver`.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to write to.
    /// * `index` - The index to read from.
    /// * `config` - The config file.
    /// * `parent` - The parent snapshot to use.
    /// * `snap` - The `SnapshotFile` to write to.
    ///
    /// # Errors
    ///
    /// * If sending the message to the raw packer fails.
    /// * If converting the data length to u64 fails
    pub fn new(
        be: BE,
        index: &'a I,
        src: &'a R,
        config: &ConfigFile,
        parent: Parent,
        mut snap: SnapshotFile,
    ) -> RusticResult<Self> {
        let indexer = Indexer::new(be.clone()).into_shared();
        let mut summary = snap.summary.take().unwrap_or_default();
        summary.backup_start = Zoned::now();

        let file_archiver = FileArchiver::new(be.clone(), index, indexer.clone(), config, src)?;
        let tree_archiver = TreeArchiver::new(be.clone(), index, indexer.clone(), config, summary)?;

        Ok(Self {
            file_archiver,
            tree_archiver,
            parent,
            indexer,
            be,
            index,
            snap,
            src,
        })
    }
    /// Archives the given source.
    ///
    /// This will archive all files and trees in the given source.
    ///
    /// # Type Parameters
    ///
    /// * `R` - The type of the source.
    ///
    /// # Arguments
    ///
    /// * `index` - The index to read from.
    /// * `src` - The source to archive.
    /// * `as_path` - The path to archive the backup as.
    /// * `skip_identical_parent` - skip saving of snapshot if tree is identical to parent tree.
    /// * `p` - The progress bar.
    ///
    /// # Errors
    ///
    /// * If sending the message to the raw packer fails.
    /// * If the index file could not be serialized.
    /// * If the time is not in the range of `Local::now()`.
    pub fn archive(
        mut self,
        src: ListAdapter<'_, R>,
        as_path: Option<&PathBuf>,
        skip_identical_parent: bool,
        no_scan: bool,
        p: &Progress,
        token: CancelToken,
    ) -> RusticResult<SnapshotFile> {
        token.check()?;
        scope(|s| -> RusticResult<_> {
            // filter out errors and handle as_path; lazily grow the
            // progress bar's length as files are discovered, since src
            // is single-pass and can't be scanned twice anymore.
            let track_size = !no_scan && !p.is_hidden();
            let mut total_size: u64 = 0;
            let iter = src.filter_map(move |item| match item {
                Err(err) => {
                    warn!("ignoring error: {}", err.to_string());
                    None
                }
                Ok(file) => {
                    if track_size {
                        total_size += file.size();
                        p.set_length(total_size);
                    }

                    let is_dir = file.is_dir();
                    // Only files need to be reopened for content later;
                    // dirs (and anything else) carry no source path.
                    let src_path = (!is_dir).then(|| file.path().to_path_buf());
                    let path = file.path();
                    let snapshot_path = if let Some(as_path) = as_path {
                        crate::join_force(as_path, path)
                    } else {
                        path.to_path_buf()
                    };

                    // File -> Node, dropping File's own path since we pair
                    // the node with `snapshot_path` (which may be remapped).
                    let (_, node) = file.into_tree();
                    Some(if is_dir {
                        (snapshot_path, node, src_path)
                    } else {
                        (
                            snapshot_path
                                .parent()
                                .unwrap_or(Path::new(""))
                                .to_path_buf(),
                            node,
                            src_path,
                        )
                    })
                }
            });

            // handle beginning and ending of trees
            let iter = TreeIterator::new(iter);

            // use parent snapshot
            iter.filter_map(
                |item| match self.parent.process(&self.be, self.index, item) {
                    Ok(item) => Some(item),
                    Err(err) => {
                        warn!("ignoring error reading parent snapshot: {err:?}");
                        None
                    }
                },
            )
            // archive files in parallel — check before each unit of work so
            // in-flight threads drain quickly once canceled. Note: errors here
            // are swallowed by the filter_map below; the definitive stop is in
            // try_for_each.
            .parallel_map_scoped(s, |item| {
                token.check()?;
                self.file_archiver.process(item, p)
            })
            .readahead_scoped(s)
            .filter_map(|item| match item {
                Ok(item) => Some(item),
                Err(err) => {
                    warn!("ignoring error: {}", err.display_log());
                    None
                }
            })
            // This is where cancellation errors actually propagate and unwind
            // the pipeline — the check here is the authoritative stop point.
            .try_for_each(|item| {
                token.check()?;
                self.tree_archiver.add(item)
            })?;

            Ok(())
        })?;

        // Guard against cancellation that arrived while the scope was draining.
        // We don't want to write a partial snapshot to the backend.
        token.check()?;

        let stats = self.file_archiver.finalize()?;
        let (id, mut summary) = self.tree_archiver.finalize(self.parent.tree_id())?;
        stats.apply(&mut summary, BlobType::Data);
        self.snap.tree = id;

        self.indexer.write().unwrap().finalize()?;

        summary.finalize(&self.snap.time);
        self.snap.summary = Some(summary);

        if !skip_identical_parent || Some(self.snap.tree) != self.parent.tree_id() {
            let id = self.be.save_file(&self.snap)?;
            self.snap.id = id.into();
        }

        p.finish();
        Ok(self.snap)
    }
}
