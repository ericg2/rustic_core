//! `backup` subcommand.
//!
//! This module provides the configuration and builder used to create and run
//! backups from one or more [`ReadSource`] implementations.
//!
//! A [`BackupBuilder`] borrows its sources rather than owning them. This means
//! source implementations do not need to be wrapped in `Arc` merely because
//! multiple [`SourceRoot`] values refer to the same source. A single source
//! can safely be used for multiple roots as long as it lives for the duration
//! of the backup.
//!
//! Multiple source implementations can still be combined into one backup.
//! [`MultiSource`] performs the path-based dispatch, while [`ListAdapter`] and
//! [`Archiver`] continue to operate on the resulting combined source.

use derive_setters::Setters;
use itertools::Itertools;
use log::info;
use path_dedot::ParseDot;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use std::{path::PathBuf};

use crate::{
    archiver::Archiver,
    archiver::parent::Parent,
    backend::dry_run::DryRunBackend,
    backend::multi::{MultiSource, SourceRoot},
    error::{ErrorKind, RusticError, RusticResult},
    repofile::{
        snapshotfile::{
            grouping::{SnapshotGroup, SnapshotGroupCriterion},
            SnapshotId,
        },
        SnapshotFile,
    },
    repository::{IndexedIds, IndexedTree, Repository},
    CancelToken, ListAdapter, ListOptions, ReadSource,
};

#[cfg(feature = "clap")]
use clap::ValueHint;

/// Options controlling how a backup selects and uses a parent snapshot.
///
/// By default, backups look for the most recent snapshot matching the
/// configured grouping criteria. A parent can instead be explicitly
/// specified, or parent selection can be disabled entirely with [`Self::force`].
///
/// The options also control which metadata changes are ignored when comparing
/// files against a parent snapshot.
#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[allow(clippy::struct_excessive_bools)]
#[non_exhaustive]
pub struct ParentOptions {
    /// Groups snapshots by the selected criteria when automatically finding a
    /// suitable parent.
    ///
    /// The default grouping is `host,label,paths`.
    #[cfg_attr(
        feature = "clap",
        clap(long, short = 'g', value_name = "CRITERION")
    )]
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[cfg_attr(
        feature = "merge",
        merge(strategy = conflate::option::overwrite_none)
    )]
    pub group_by: Option<SnapshotGroupCriterion>,

    /// Explicit snapshot IDs to use as parents.
    ///
    /// This option can be specified multiple times. When provided, automatic
    /// parent selection is disabled.
    #[cfg_attr(
        feature = "clap",
        clap(
            long = "parent",
            value_name = "SNAPSHOT",
            conflicts_with = "force"
        )
    )]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::vec::append))]
    pub parents: Vec<String>,

    /// Skips writing the snapshot when nothing has changed relative to its
    /// parent snapshot.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(
        feature = "merge",
        merge(strategy = conflate::bool::overwrite_false)
    )]
    pub skip_if_unchanged: bool,

    /// Disables parent selection and causes all files to be read.
    ///
    /// This conflicts with explicitly selected parents and the options that
    /// modify parent-based file comparison.
    #[cfg_attr(feature = "clap", clap(long, short))]
    #[cfg_attr(
        feature = "merge",
        merge(strategy = conflate::bool::overwrite_false)
    )]
    pub force: bool,

    /// Ignores ctime changes when determining whether a file has changed.
    #[cfg_attr(feature = "clap", clap(long, conflicts_with = "force"))]
    #[cfg_attr(
        feature = "merge",
        merge(strategy = conflate::bool::overwrite_false)
    )]
    pub ignore_ctime: bool,

    /// Ignores inode-number changes when determining whether a file has
    /// changed.
    #[cfg_attr(feature = "clap", clap(long, conflicts_with = "force"))]
    #[cfg_attr(
        feature = "merge",
        merge(strategy = conflate::bool::overwrite_false)
    )]
    pub ignore_inode: bool,
}

impl ParentOptions {
    /// Finds the parent snapshot(s) for `snap` and constructs the corresponding
    /// [`Parent`] object used by the archiver.
    ///
    /// If [`Self::force`] is enabled, no parent is selected.
    ///
    /// If explicit parents were supplied through [`Self::parents`], those
    /// snapshots are used.
    ///
    /// Otherwise, the most recent snapshot matching [`Self::group_by`] is
    /// selected.
    ///
    /// The returned vector contains the selected snapshot IDs in the same
    /// order used to construct the parent.
    ///
    /// # Arguments
    ///
    /// * `repo` - Repository containing the candidate snapshots and trees.
    /// * `snap` - Snapshot currently being created.
    ///
    /// # Returns
    ///
    /// The selected parent snapshot IDs together with the [`Parent`] object
    /// used for change detection.
    pub(crate) fn get_parent<S: IndexedTree>(
        &self,
        repo: &Repository<S>,
        snap: &SnapshotFile,
    ) -> (Vec<SnapshotId>, Parent) {
        let group =
            SnapshotGroup::from_snapshot(snap, self.group_by.unwrap_or_default());

        let parent = if self.force {
            Vec::new()
        } else if self.parents.is_empty() {
            SnapshotFile::latest(
                repo.dbe(),
                |snap| group.matches(snap),
                &repo.progress_counter(""),
            )
                .ok()
                .into_iter()
                .collect()
        } else {
            SnapshotFile::from_strs(
                repo.dbe(),
                &self.parents,
                |snap| group.matches(snap),
                &repo.progress_counter(""),
            )
                .unwrap_or_default()
        };

        let (parent_trees, parent_ids): (Vec<_>, _) = parent
            .into_iter()
            .map(|parent| (parent.tree, parent.id))
            .unzip();

        (
            parent_ids,
            Parent::new(
                repo.dbe(),
                repo.index(),
                parent_trees,
                self.ignore_ctime,
                self.ignore_inode,
            ),
        )
    }
}

/// Options controlling the `backup` command.
///
/// This type contains the command-line/configuration options for a backup.
/// Sources themselves are supplied separately through [`BackupBuilder`].
///
/// The structure intentionally remains compatible with the existing builder,
/// command-line parsing, configuration deserialization, and
/// `conflate::Merge` behavior.
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
pub struct BackupOptions {
    /// Overrides the paths stored in the resulting snapshot.
    ///
    /// The actual source roots are still used for reading data. This option
    /// only controls the path recorded in the snapshot.
    #[cfg_attr(
        feature = "clap",
        clap(
            long,
            value_name = "PATH",
            value_hint = ValueHint::DirPath
        )
    )]
    #[cfg_attr(
        feature = "merge",
        merge(strategy = conflate::option::overwrite_none)
    )]
    pub as_path: Option<PathBuf>,

    /// Avoids scanning the backup source for its size.
    ///
    /// Disabling the scan also disables ETA estimation for the backup.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(
        feature = "merge",
        merge(strategy = conflate::bool::overwrite_false)
    )]
    pub no_scan: bool,

    /// Runs the backup without writing data or a snapshot.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(
        feature = "merge",
        merge(strategy = conflate::bool::overwrite_false)
    )]
    pub dry_run: bool,

    /// Controls parent snapshot selection and comparison behavior.
    #[cfg_attr(feature = "clap", clap(flatten))]
    #[serde(flatten)]
    pub parent_opts: ParentOptions,

    /// Controls how the backup sources are listed.
    #[cfg_attr(feature = "clap", clap(flatten))]
    #[serde(flatten)]
    pub source_opts: ListOptions,
}

/// Builder for running a backup against one or more source roots.
///
/// A builder borrows every [`ReadSource`] that it uses. Sources therefore do
/// not need to be placed inside `Arc` simply to allow multiple source roots to
/// refer to the same backend.
///
/// A source may be added more than once with different roots, and different
/// source implementations may be mixed in the same backup. The configured
/// roots are combined into a single [`MultiSource`] before listing and
/// archiving begins.
///
/// The builder owns the [`BackupOptions`], [`SnapshotFile`], and
/// [`CancelToken`], but only borrows the repository and source implementations.
///
/// # Example
///
/// ```ignore
/// let source = MySource::new(...);
///
/// repository
///     .backup_builder(snapshot)
///     .add_source(&source)
///     .run()?;
/// # Ok::<(), rustic_core::error::RusticError>(())
/// ```
///
/// For multiple roots backed by the same source:
///
/// ```ignore
/// let source = MySource::new(...);
///
/// repository
///     .backup_builder(snapshot)
///     .add_multi(&source, ["/home", "/etc"])
///     .run()?;
/// # Ok::<(), rustic_core::error::RusticError>(())
/// ```
#[allow(missing_debug_implementations)]
pub struct BackupBuilder<'a, S> {
    /// Repository used for reading configuration, indexes, snapshots, and
    /// writing the resulting backup.
    repo: &'a Repository<S>,

    /// Options controlling how the backup is performed.
    opts: BackupOptions,

    /// Source roots that will be combined into the backup source.
    sources: Vec<SourceRoot<'a>>,

    /// Snapshot that will be populated and written by the backup.
    snap: SnapshotFile,

    /// Cancellation token checked by the backup pipeline.
    token: CancelToken,
}

impl<'a, S: IndexedIds> BackupBuilder<'a, S> {
    /// Creates a new backup builder for `snap`.
    ///
    /// The builder starts with default [`BackupOptions`], no sources, and a
    /// newly-created [`CancelToken`].
    ///
    /// Prefer [`Repository::backup_builder`] over calling this method
    /// directly.
    pub(crate) fn new(repo: &'a Repository<S>, snap: SnapshotFile) -> Self {
        Self {
            repo,
            opts: BackupOptions::default(),
            sources: Vec::new(),
            snap,
            token: CancelToken::new(),
        }
    }

    /// Replaces the backup options.
    ///
    /// This method consumes and returns the builder so calls can be chained.
    #[must_use]
    pub fn options(mut self, opts: BackupOptions) -> Self {
        self.opts = opts;
        self
    }

    /// Adds `source` as a source rooted at `/`.
    ///
    /// The source is borrowed for the lifetime of the builder and is not
    /// cloned, reference-counted, or otherwise owned by the builder.
    ///
    /// The same source can be added multiple times with different roots using
    /// [`Self::add_source_path`] or [`Self::add_multi`].
    #[must_use]
    pub fn add_source(mut self, source: &'a dyn ReadSource) -> Self {
        self.sources.push(SourceRoot::new(source, "/"));
        self
    }

    /// Adds `source` using `path` as its source root.
    ///
    /// The source must outlive the builder because the builder stores a
    /// borrowed reference to it.
    #[must_use]
    pub fn add_source_path(
        mut self,
        source: &'a dyn ReadSource,
        path: impl Into<PathBuf>,
    ) -> Self {
        self.sources
            .push(SourceRoot::new(source, path.into()));
        self
    }

    /// Adds multiple source roots backed by the same source.
    ///
    /// The source is borrowed once and referenced by every resulting
    /// [`SourceRoot`]. No `Arc` or other shared-ownership mechanism is needed.
    ///
    /// This is useful when several independent paths should be read from the
    /// same backend.
    #[must_use]
    pub fn add_multi<I, P>(
        mut self,
        source: &'a dyn ReadSource,
        roots: I,
    ) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.sources.extend(
            roots
                .into_iter()
                .map(|path| SourceRoot::new(source, path.into())),
        );
        self
    }

    /// Adds several already-configured source roots.
    ///
    /// This is the method to use when different roots need to be backed by
    /// different [`ReadSource`] implementations.
    ///
    /// Each [`SourceRoot`] must contain a source borrowed for the lifetime of
    /// this builder.
    #[must_use]
    pub fn add_sources(
        mut self,
        sources: impl IntoIterator<Item = SourceRoot<'a>>,
    ) -> Self {
        self.sources.extend(sources);
        self
    }

    /// Replaces the cancellation token used by the backup.
    ///
    /// The supplied token is passed through to the archiving operation, which
    /// checks it while the backup is running.
    #[must_use]
    pub fn with_token(mut self, token: CancelToken) -> Self {
        self.token = token;
        self
    }

    /// Runs the configured backup and returns the saved snapshot.
    ///
    /// The configured source roots are first combined into a [`MultiSource`].
    /// The resulting source is then used by both [`ListAdapter`] and
    /// [`Archiver`], so neither component needs to know that multiple source
    /// roots were configured.
    ///
    /// If `as_path` is configured, it changes the paths recorded in the
    /// resulting snapshot without changing the actual source roots used to
    /// read the data.
    ///
    /// Parent selection is performed before archiving. When a parent is
    /// selected, its ID is recorded in the resulting snapshot and its tree is
    /// supplied to the archiver for change detection.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    ///
    /// * no source roots have been added;
    /// * the configured `as_path` cannot be parsed;
    /// * the snapshot paths cannot be updated;
    /// * the configured sources cannot be combined;
    /// * source listing fails;
    /// * parent lookup fails internally;
    /// * or the archiving operation fails.
    pub fn run(self) -> RusticResult<SnapshotFile> {
        let Self {
            repo,
            opts,
            sources,
            mut snap,
            token,
        } = self;

        if sources.is_empty() {
            return Err(RusticError::new(
                ErrorKind::InvalidInput,
                "BackupBuilder: at least one source must be added via `.add_source(..)`, `.add_multi(..)`, or `.add_sources(..)` before calling `.run()`",
            ));
        }

        let backup_paths: Vec<PathBuf> =
            sources.iter().map(|source| source.path.clone()).collect();

        let src = MultiSource::new(sources).map_err(|err| {
            RusticError::with_source(
                ErrorKind::InvalidInput,
                "Failed to combine backup sources",
                err,
            )
        })?;

        let index = repo.index();

        let as_path = opts
            .as_path
            .as_ref()
            .map(|path| -> RusticResult<_> {
                Ok(path
                    .parse_dot()
                    .map_err(|err| {
                        RusticError::with_source(
                            ErrorKind::InvalidInput,
                            "Failed to parse dotted path `{path}`",
                            err,
                        )
                            .attach_context(
                                "path",
                                path.display().to_string(),
                            )
                    })?
                    .to_path_buf())
            })
            .transpose()?;

        let paths = as_path.as_ref().map_or(
            backup_paths.as_slice(),
            std::slice::from_ref,
        );

        snap.paths.set_paths(paths).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Internal,
                "Failed to set paths `{paths}` in snapshot.",
                err,
            )
                .attach_context(
                    "paths",
                    backup_paths
                        .iter()
                        .map(|path| path.display().to_string())
                        .join(","),
                )
        })?;

        let (parent_ids, parent) =
            opts.parent_opts.get_parent(repo, &snap);

        if parent_ids.is_empty() {
            info!("using no parent");
        } else {
            info!("using parents {}", parent_ids.iter().join(", "));
            snap.parent = Some(parent_ids[0]);
            snap.parents = parent_ids;
        }

        let be =
            DryRunBackend::new(repo.dbe().clone(), opts.dry_run);

        info!("starting to backup {backup_paths:?} ...");

        let archiver =
            Archiver::new(be, index, &src, repo.config(), parent, snap)?;

        let progress = repo.progress_bytes("backing up...");

        let lister = ListAdapter::builder(&src)
            .with_roots(backup_paths.clone())
            .with_options(opts.source_opts.clone())
            .build()
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Internal,
                    "Failed to list `{paths}` in snapshot.",
                    err,
                )
                    .attach_context(
                        "paths",
                        backup_paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .join(","),
                    )
            })?;

        archiver.archive(
            lister,
            as_path.as_ref(),
            opts.parent_opts.skip_if_unchanged,
            opts.no_scan,
            &progress,
            token,
        )
    }
}