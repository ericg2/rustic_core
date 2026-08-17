use std::{
    collections::VecDeque,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{File, FilterOptions, ListOptions, Metadata, Node, NodeType, ReadSource, WriteSource, backend::filters::{ExcludeFilter, GitIgnoreFilter}};

/// A single pending directory to expand, along with the ancestor chain of
/// already-loaded gitignore directories (top-down) needed to filter its
/// children.
#[derive(Clone, Debug)]
struct PendingDir {
    path: PathBuf,
    gitignore_ancestors: Vec<PathBuf>,
    /// Expected device id for entries in this directory, when
    /// `one_file_system` is enabled (`None` = check disabled). Propagated
    /// from parent to child as directories are expanded, so this always
    /// reflects the *immediate parent's* device id — matching
    /// `ignore::WalkBuilder::same_file_system`'s per-directory boundary
    /// semantics rather than a single fixed comparison against the walk
    /// root.
    device_id: Option<u64>,
}

/// Iterator adapter that lists (optionally recursively) the contents of a
/// [`WriteSource`], applying [`FilterOptions`] and glob/gitignore excludes.
///
/// This is conceptually similar to a local `ignore::WalkBuilder`-based
/// walker, but drives its own BFS traversal and its own gitignore/override
/// matching by hand, since it must work over an arbitrary backend
/// (including virtualized/non-filesystem sources) rather than `std::fs`.
///
/// All [`FilterOptions`] fields are honored:
/// - `git_ignore` / `no_require_git` / `custom_ignorefiles`: see
///   [`GitignoreFilter`], which also honors `.git/info/exclude` at the
///   repo root.
/// - `exclude_if_present`: any marker file directly inside a directory
///   excludes that directory's entire contents (children never
///   emitted or descended into).
/// - `exclude_if_xattr`: entries with a matching extended attribute name
///   (as reported by the backend's [`Metadata::extended_attributes`]) are
///   excluded.
/// - `exclude_larger_than`: non-directory entries larger than this are
///   excluded.
/// - `one_file_system`: entries whose `Metadata::device_id` differs from
///   their parent directory's device id are excluded (and, if a
///   directory, never descended into). The root's own device id is
///   captured once at construction time and used as the initial parent
///   device id.
///
/// Sibling entries within a directory are yielded in sorted path order,
/// matching `sort_by_file_path(Path::cmp)` on a local `ignore` walker,
/// while still streaming: only one directory's children are ever held in
/// memory at a time.
///
/// Symlinks are assumed to be reported as-is (not followed) by the
/// backend's `readdir`/`Node` implementation; this adapter does not
/// itself perform any symlink resolution.
pub struct ListAdapter {
    be: Arc<dyn WriteSource>,
    root: PathBuf,
    recursive: bool,
    excludes: Option<ExcludeFilter>,
    gitignore: GitIgnoreFilter,
    filter_opts: FilterOptions,
    dirs: VecDeque<PendingDir>,
    current_batch: VecDeque<File>,
}

impl ListAdapter {
    /// Creates a new lister rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if `root` cannot be stat'd (only required when
    /// `one_file_system` is enabled) or if the initial gitignore layer
    /// fails to load.
    pub fn new(be: Arc<dyn WriteSource>, root: impl AsRef<Path>, opts: ListOptions) -> io::Result<Self> {
        let root = root.as_ref();
        let filter_opts = opts.filters.unwrap_or_default();

        let excludes = match opts.excludes {
            Some(ref ex) if !ex.is_empty() => Some(ExcludeFilter::new(ex)?),
            _ => None,
        };

        let read: Arc<dyn ReadSource> = be.clone();
        let gitignore = GitIgnoreFilter::new(read, &filter_opts);
        if gitignore.enabled() {
            gitignore.load_dir(root)?;
        }

        let root_device_id = if filter_opts.one_file_system {
            let meta = be.stat(root)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("root path `{}` does not exist", root.display()),
                )
            })?;
            Some(meta.device_id)
        } else {
            None
        };

        let mut dirs = VecDeque::new();
        dirs.push_back(PendingDir {
            path: root.to_path_buf(),
            gitignore_ancestors: vec![root.to_path_buf()],
            device_id: root_device_id,
        });

        Ok(Self {
            be,
            root: root.to_path_buf(),
            recursive: opts.recursive,
            excludes,
            gitignore,
            filter_opts,
            dirs,
            current_batch: VecDeque::new(),
        })
    }

    /// Returns the root path this lister was constructed with.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lists the immediate (non-recursive) children of `dir`, converts
    /// them into [`File`]s, and applies every metadata-based filter
    /// (`exclude_if_present`, `exclude_if_xattr`, `exclude_larger_than`,
    /// `one_file_system`) inline, before the caller applies path-based
    /// (glob / gitignore) filters. Results are sorted by path for
    /// deterministic sibling order.
    ///
    /// `expected_device_id` is the device id children must match when
    /// `one_file_system` is enabled (i.e. `dir`'s own device id, as
    /// established when `dir` itself was accepted).
    fn list_dir_children(
        &self,
        dir: &Path,
        expected_device_id: Option<u64>,
    ) -> io::Result<Vec<File>> {
        // If any `exclude_if_present` marker exists directly inside `dir`,
        // the entire directory's contents are excluded (children never
        // emitted or descended into).
        for marker in &self.filter_opts.exclude_if_present {
            if self.be.stat(&dir.join(marker))?.is_some() {
                return Ok(Vec::new());
            }
        }

        let entries = self.be.readdir(dir)?;
        let mut out = Vec::new();
        for entry in entries {
            let node: Node = entry?;
            let name = node.name();

            // The directory listing itself (some backends yield a "." /
            // empty-name entry for the dir being listed) — skip it.
            if name.trim_end_matches('/').is_empty() {
                continue;
            }

            if !self.keep_by_metadata(&node.metadata, node.node_type, expected_device_id) {
                continue;
            }

            let child_path = dir.join(name);
            let file = File::new(
                child_path,
                node.node_type,
                node.metadata,
                Some(Arc::clone(&self.be) as Arc<dyn ReadSource>),
                Some(Arc::clone(&self.be) as Arc<dyn WriteSource>),
            )?;

            out.push(file);
        }

        // Deterministic sibling order, matching `sort_by_file_path`
        // (`Path::cmp`) on a local `ignore`-crate walker. This sorts only
        // the children of a single directory (already fully materialized
        // above), so the adapter still streams overall: no more than one
        // directory's worth of entries is ever held in memory at once.
        out.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(out)
    }

    /// Applies the purely metadata-based filters: `exclude_if_xattr`,
    /// `exclude_larger_than`, and `one_file_system`.
    fn keep_by_metadata(
        &self,
        metadata: &Metadata,
        node_type: NodeType,
        expected_device_id: Option<u64>,
    ) -> bool {
        if !self.filter_opts.exclude_if_xattr.is_empty()
            && metadata.extended_attributes.iter().any(|xattr| {
                self.filter_opts
                    .exclude_if_xattr
                    .iter()
                    .any(|excluded| excluded == &xattr.name)
            })
        {
            return false;
        }

        if node_type != NodeType::Dir {
            if let Some(max) = self.filter_opts.exclude_larger_than {
                if metadata.size > max.as_u64() {
                    return false;
                }
            }
        }

        if let Some(expected) = expected_device_id {
            if metadata.device_id != expected {
                return false;
            }
        }

        true
    }
}

impl Iterator for ListAdapter {
    type Item = io::Result<File>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(file) = self.current_batch.pop_front() {
                return Some(Ok(file));
            }

            let pending = self.dirs.pop_front()?;
            let children = match self.list_dir_children(&pending.path, pending.device_id) {
                Ok(c) => c,
                Err(err) => return Some(Err(err)),
            };

            for child in children {
                // 1. Exclude-glob filtering (top-down, cheap, cached).
                if let Some(ex) = &self.excludes {
                    if !ex.is_ok(&child) {
                        continue;
                    }
                }

                // 2. Gitignore filtering, using the ancestor chain of
                //    already-loaded directories.
                if self.gitignore.enabled()
                    && !self.gitignore.is_ok(&child, &pending.gitignore_ancestors)
                {
                    continue;
                }

                if child.is_dir() && self.recursive {
                    let mut ancestors = pending.gitignore_ancestors.clone();
                    if self.gitignore.enabled() {
                        if let Err(err) = self.gitignore.load_dir(child.path()) {
                            return Some(Err(err));
                        }
                        ancestors.push(child.path().to_path_buf());
                    }
                    self.dirs.push_back(PendingDir {
                        path: child.path().to_path_buf(),
                        gitignore_ancestors: ancestors,
                        // The child already passed the device-id check in
                        // `keep_by_metadata` above (or the check is
                        // disabled), so its device id equals
                        // `pending.device_id` — propagate as-is rather
                        // than re-stat'ing.
                        device_id: pending.device_id,
                    });
                }

                self.current_batch.push_back(child);
            }

            // Loop again: either we now have a batch to drain, or this
            // directory was empty/fully filtered and we move to the next
            // pending directory.
        }
    }
}
