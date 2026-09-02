use std::{
    collections::{HashSet, VecDeque},
    io,
    path::{Path, PathBuf},
};

use crate::backend::filters::GitignoreLayers;
use crate::{
    File, FilterOptions, ListOptions, Node, NodeType, ReadSource, WriteSource,
    backend::filters::ExcludeFilter,
};

/// Walks deeper than this are treated as a symlink loop or pathological
/// input and aborted with an error, rather than running forever.
const MAX_DEPTH: usize = 10_000;

/// Normalizes a path to use `/` as its separator, regardless of platform.
///
/// Applied unconditionally (not just under `cfg(windows)`) to every path
/// that enters or is constructed by this module — caller-supplied roots,
/// and every `child_path` built during a directory listing — so output
/// paths are always `/`-separated no matter what platform this crate is
/// built for, and no matter what separator style a caller-supplied root
/// happened to use. This keeps `child_path == dir` comparisons,
/// `visited_dirs` membership, and gitignore matching all operating on a
/// single consistent representation instead of needing separator-aware
/// logic scattered around.
fn to_forward_slash(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.contains('\\') {
        PathBuf::from(s.replace('\\', "/"))
    } else {
        path.to_path_buf()
    }
}

/// A directory queued for expansion, plus the gitignore ancestor chain
/// needed to filter its children.
#[derive(Clone, Debug)]
struct PendingDir {
    /// Absolute path of the directory to expand.
    path: PathBuf,
    /// Chain of ancestor directories (from the owning root down to
    /// `path`'s parent) whose gitignore rules apply to this directory's
    /// children, in order.
    gitignore_ancestors: Vec<PathBuf>,
    /// Device id of the root this directory descends from, used to
    /// enforce `one_file_system` filtering. `None` when that filter is
    /// disabled.
    device_id: Option<u64>,
    /// Depth of `path` below its owning root (root itself is depth 0).
    depth: usize,
}

/// Lists (optionally recursively) the contents of one or more
/// [`WriteSource`] paths, applying [`FilterOptions`] and glob/gitignore
/// excludes.
///
/// Matches the filtering behavior of `ignore::WalkBuilder`, including
/// `.gitignore`, `.git/info/exclude`, and custom ignore files, but drives
/// it against an arbitrary [`ReadSource`] backend instead of `std::fs`.
///
/// When constructed with multiple roots, each root is walked
/// independently with its own gitignore ancestor chain and (if
/// `one_file_system` is set) its own device id, but all roots share a
/// single symlink-cycle guard so a directory reachable from two roots is
/// only descended into once.
///
/// All paths — both caller-supplied roots and every path yielded by
/// iteration — are normalized to use `/` as a separator; see
/// [`to_forward_slash`].
pub struct ListAdapter<'a, R: ReadSource> {
    be: &'a R,
    roots: Vec<PathBuf>,
    recursive: bool,
    filter_opts: FilterOptions,
    excludes: Option<ExcludeFilter>,
    gitignore: GitignoreLayers<'a, R>,

    dirs: VecDeque<PendingDir>,
    current_batch: VecDeque<File>,
    /// Guards against symlink cycles when the backend reports a
    /// symlinked directory as a plain directory, and against revisiting
    /// a directory reachable from more than one root.
    visited_dirs: HashSet<PathBuf>,
}

impl<'a, R: ReadSource> ListAdapter<'a, R> {
    /// Returns the backend this lister reads from.
    ///
    /// # Returns
    ///
    /// A reference to the [`ReadSource`] backend supplied at construction.
    pub fn rbe(&self) -> &'a R {
        &self.be
    }

    /// Creates a lister rooted at a single `root`, using default
    /// [`ListOptions`].
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to read directory entries and metadata from.
    /// * `root` - The single path to walk.
    ///
    /// # Errors
    ///
    /// * If `root` cannot be stat'd or read from the backend.
    ///
    /// # Returns
    ///
    /// A [`ListAdapter`] ready to be iterated.
    pub fn new(be: &'a R, root: impl AsRef<Path>) -> io::Result<Self> {
        Self::with_options(be, root, ListOptions::default())
    }

    /// Creates a lister rooted at a single `root` with custom
    /// [`ListOptions`].
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to read directory entries and metadata from.
    /// * `root` - The single path to walk.
    /// * `opts` - Filtering, exclude, and recursion options.
    ///
    /// # Errors
    ///
    /// * If `root` does not exist or cannot be stat'd (when
    ///   `one_file_system` is enabled).
    /// * If `opts.excludes` contains an invalid glob pattern.
    ///
    /// # Returns
    ///
    /// A [`ListAdapter`] ready to be iterated.
    pub fn with_options(be: &'a R, root: impl AsRef<Path>, opts: ListOptions) -> io::Result<Self> {
        Self::with_options_multi(be, [root], opts)
    }

    /// Creates a lister that walks several roots, using default [`ListOptions`].
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to read directory entries and metadata from.
    /// * `roots` - The paths to walk, in iteration order.
    ///
    /// # Errors
    ///
    /// * If any root cannot be stat'd or read from the backend.
    ///
    /// # Returns
    ///
    /// A [`ListAdapter`] ready to be iterated.
    pub fn new_multi<I, P>(be: &'a R, roots: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        Self::with_options_multi(be, roots, ListOptions::default())
    }

    /// Creates a lister that walks several roots with custom
    /// [`ListOptions`].
    ///
    /// Roots are walked in the order given. Each root gets its own
    /// gitignore ancestor chain (so a `.gitignore` above one root has no
    /// effect on another), but the symlink-cycle guard is shared across
    /// all roots, so a directory reachable from more than one root is
    /// only yielded once, from whichever root reaches it first.
    ///
    /// Roots are normalized to use `/` separators before being stored or
    /// used, regardless of what separator style they're passed in with.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to read directory entries and metadata from.
    /// * `roots` - The paths to walk, in iteration order.
    /// * `opts` - Filtering, exclude, and recursion options.
    ///
    /// # Errors
    ///
    /// * If any root does not exist or cannot be stat'd (when
    ///   `one_file_system` is enabled).
    /// * If `opts.excludes` contains an invalid glob pattern.
    ///
    /// # Returns
    ///
    /// A [`ListAdapter`] ready to be iterated.
    pub fn with_options_multi<I, P>(be: &'a R, roots: I, opts: ListOptions) -> io::Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let roots: Vec<PathBuf> = roots
            .into_iter()
            .map(|p| to_forward_slash(p.as_ref()))
            .collect();

        let filter_opts = opts.filters.unwrap_or_default();
        let excludes = match opts.excludes {
            Some(ref ex) if !ex.is_empty() => Some(ExcludeFilter::new(ex)?),
            _ => None,
        };

        let mut gitignore = GitignoreLayers::new(be, &filter_opts);
        let mut dirs = VecDeque::new();
        let mut visited_dirs = HashSet::new();
        for root in &roots {
            let device_id = if filter_opts.one_file_system {
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

            if gitignore.enabled() {
                gitignore.load_dir(root)?;
            }

            // Only queue this root if it hasn't already been reached via
            // an earlier root (e.g. duplicate or nested roots).
            if visited_dirs.insert(root.clone()) {
                dirs.push_back(PendingDir {
                    path: root.clone(),
                    gitignore_ancestors: vec![root.clone()],
                    device_id,
                    depth: 0,
                });
            }
        }

        Ok(Self {
            be,
            roots,
            filter_opts,
            excludes,
            gitignore,
            dirs,
            visited_dirs,
            current_batch: VecDeque::new(),
            recursive: !opts.no_recursive,
        })
    }

    /// The first root path this lister was constructed with.
    ///
    /// For listers constructed from multiple roots, prefer [`Self::roots`].
    ///
    /// # Returns
    ///
    /// A reference to the first root path.
    pub fn root(&self) -> &Path {
        &self.roots[0]
    }

    /// All root paths this lister was constructed with, in walk order.
    ///
    /// # Returns
    ///
    /// A slice over every root path passed at construction.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Lists and filters the immediate children of `dir`, sorted by path.
    ///
    /// Applies `exclude_if_present` marker files, `exclude_if_xattr`,
    /// `exclude_larger_than`, and (when set) `expected_device_id`
    /// filtering. Does not apply glob excludes or gitignore rules; those
    /// are applied by the caller.
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory whose children should be listed.
    /// * `expected_device_id` - When `Some`, children on a different
    ///   device are skipped (implements `one_file_system`).
    ///
    /// # Errors
    ///
    /// * If the backend fails to stat a marker file or read `dir`.
    /// * If constructing a [`File`] from a listed entry fails.
    ///
    /// # Returns
    ///
    /// The filtered, path-sorted children of `dir`.
    fn list_dir_children(
        &self,
        dir: &Path,
        expected_device_id: Option<u64>,
    ) -> io::Result<Vec<File>> {
        for marker in &self.filter_opts.exclude_if_present {
            if self.be.stat(&dir.join(marker))?.is_some() {
                return Ok(Vec::new());
            }
        }

        let mut out = Vec::with_capacity(32);
        for entry in self.be.readdir(dir)? {
            let node: Node = entry?;
            let name = node.name().to_string_lossy().to_string();
            if name.trim_end_matches('/').is_empty() {
                continue;
            }

            // Normalized immediately after construction so every
            // downstream use of this path — the self-entry comparison
            // below, `File::path()`, `visited_dirs`, gitignore ancestor
            // chains — sees a `/`-separated path regardless of platform
            // or of what separator `dir` itself happened to carry.
            let p = crate::join_force(&dir, &name);
            let child_path = to_forward_slash(&p);

            // Some backends yield an entry representing the queried directory
            // itself as part of its own listing (a "self-entry"). Without this
            // guard that produces a fabricated nested path like `abcd/abcd`,
            // which — if the backend repeats the behavior one level down —
            // recurses unboundedly until MAX_DEPTH is hit.
            if child_path == dir {
                continue;
            }

            if node.meta.extended_attributes.iter().any(|xattr| {
                self.filter_opts
                    .exclude_if_xattr
                    .iter()
                    .any(|excluded| excluded == &xattr.name)
            }) {
                continue;
            }

            if node.node_type != NodeType::Dir {
                if let Some(max) = self.filter_opts.exclude_larger_than {
                    if node.meta.size > max.as_u64() {
                        continue;
                    }
                }
            }

            if expected_device_id.is_some_and(|x| x != node.meta.device_id) {
                continue;
            }

            out.push(File::new(child_path, node.node_type, node.meta)?);
        }

        out.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

impl<'a, R: ReadSource> Iterator for ListAdapter<'a, R> {
    type Item = io::Result<File>;

    /// Advances the walk, returning the next filtered [`File`] across all
    /// configured roots, or `None` once every root has been fully
    /// traversed.
    ///
    /// # Errors
    ///
    /// Yields `Some(Err(_))` if the backend fails to read a directory, a
    /// [`File`] cannot be constructed from a listed entry, or the walk
    /// exceeds [`MAX_DEPTH`] below any single root.
    ///
    /// # Returns
    ///
    /// `Some(Ok(file))` for the next entry, `Some(Err(err))` on failure,
    /// or `None` when the walk is complete.
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
                if let Some(ex) = &self.excludes {
                    if !ex.is_ok(&child) {
                        continue;
                    }
                }
                if !self
                    .gitignore
                    .is_ok(child.path(), child.is_dir(), &pending.gitignore_ancestors)
                {
                    continue;
                }

                if child.is_dir() && self.recursive {
                    if pending.depth + 1 > MAX_DEPTH {
                        return Some(Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!(
                                "max walk depth ({MAX_DEPTH}) exceeded at `{}`",
                                child.path().display()
                            ),
                        )));
                    }
                    if !self.visited_dirs.insert(child.path().to_path_buf()) {
                        // Already seen this path — symlink cycle, duplicate
                        // backend entry, or overlap between two configured
                        // roots; skip descending but still yield it.
                        self.current_batch.push_back(child);
                        continue;
                    }

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
                        device_id: pending.device_id,
                        depth: pending.depth + 1,
                    });
                }

                self.current_batch.push_back(child);
            }
        }
    }
}
