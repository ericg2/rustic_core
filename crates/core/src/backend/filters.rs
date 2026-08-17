use bytesize::ByteSize;
use derive_setters::Setters;
use serde_with::{DisplayFromStr, serde_as};

use crate::{BlockdevOption, DevIdOption, TimeOption, XattrOption};

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(serde::Deserialize, serde::Serialize, Default, Clone, Debug, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
/// [`FilterOptions`] allow to filter a source by various criteria.
pub struct FilterOptions {
    /// Ignore files based on .gitignore files
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub git_ignore: bool,

    /// Do not require a git repository to apply git-ignore rule
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub no_require_git: bool,

    /// Treat the provided filename like a .gitignore file (can be specified multiple times)
    #[cfg_attr(
        feature = "clap",
        clap(long = "custom-ignorefile", value_name = "FILE")
    )]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::vec::overwrite_empty))]
    pub custom_ignorefiles: Vec<String>,

    /// Exclude contents of directories containing this filename (can be specified multiple times)
    #[cfg_attr(feature = "clap", clap(long, value_name = "FILE"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::vec::overwrite_empty))]
    pub exclude_if_present: Vec<String>,

    /// Exclude files/directories having the given extended attribute set (can be specified multiple times)
    #[cfg_attr(feature = "clap", clap(long, value_name = "XATTR"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::vec::overwrite_empty))]
    pub exclude_if_xattr: Vec<String>,

    /// Exclude other file systems, don't cross filesystem boundaries and subvolumes
    #[cfg_attr(feature = "clap", clap(long, short = 'x'))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub one_file_system: bool,

    /// Maximum size of files to be backed up. Larger files will be excluded.
    #[cfg_attr(feature = "clap", clap(long, value_name = "SIZE"))]
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub exclude_larger_than: Option<ByteSize>,
}

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(serde::Deserialize, serde::Serialize, Default, Clone, Copy, Debug, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
/// [`SaveOptions`] describes how entries from a compatible source will be saved in the repository.
pub struct SaveOptions {
    /// Set access time [default: mtime]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_atime: Option<TimeOption>,

    /// Set changed time [default: yes]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_ctime: Option<TimeOption>,

    /// Set device ID [default: hardlink]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_devid: Option<DevIdOption>,

    /// How block devices should be stored [default: special]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_blockdev: Option<BlockdevOption>,

    /// Set extended attributes [default: yes]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_xattrs: Option<XattrOption>,
}

use dashmap::DashMap;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::Override;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::{Excludes, File, ReadSource};

/// Filters entries against a set of glob [`Excludes`].
///
/// Directory results are cached in `work_dirs` so that once a directory is
/// known to be whitelisted/ignored, all of its descendants inherit that
/// decision without re-matching every glob.
pub struct ExcludeFilter {
    overrides: Override,
    work_dirs: DashMap<PathBuf, bool>,
}

impl ExcludeFilter {
    pub fn new(excludes: &Excludes) -> io::Result<Self> {
        Ok(Self {
            overrides: excludes.as_override()?,
            work_dirs: DashMap::new(),
        })
    }

    /// Returns `true` if `file` should be kept.
    ///
    /// Must be called in top-down (parent-before-child) order, since it
    /// relies on ancestor decisions already being cached for inheritance.
    pub fn is_ok(&self, file: &File) -> bool {
        let path = file.path();
        let is_dir = file.is_dir();

        match self.overrides.matched(path, is_dir) {
            Match::Ignore(_) => {
                if is_dir {
                    let _ = self.work_dirs.insert(path.to_path_buf(), false);
                }
                return false;
            }
            Match::Whitelist(_) => {
                if is_dir {
                    let _ = self.work_dirs.insert(path.to_path_buf(), true);
                }
                return true;
            }
            Match::None => {}
        }

        // No direct match: inherit from the nearest ancestor decision, if any.
        let mut ancestor = path.parent();
        while let Some(parent) = ancestor {
            if let Some(flag) = self.work_dirs.get(parent).map(|x| *x.value()) {
                return flag;
            }
            ancestor = parent.parent();
        }

        true
    }
}

/// Filters entries against `.gitignore` (and equivalent custom ignore
/// files), fetched lazily via the backend's [`ReadSource::open_read`] /
/// [`ReadSource::stat`], so it works for any backend — not just local disk.
///
/// This intentionally does NOT use `ignore::WalkBuilder` (which only walks
/// real filesystem paths) — it drives the `ignore` crate's *matcher* types
/// directly (`Gitignore` / `GitignoreBuilder`) against content pulled
/// through the backend abstraction.
///
/// In addition to per-directory `.gitignore` files, this also honors
/// `.git/info/exclude` at the repo root (mirroring
/// `ignore::WalkBuilder::git_exclude`), and gates both of those on the
/// presence of an actual `.git` repo unless `no_require_git` is set
/// (mirroring `ignore::WalkBuilder::require_git`). `custom_ignorefiles`
/// are always honored when the filter is enabled, regardless of
/// `require_git`.
pub struct GitIgnoreFilter {
    /// The reader for the source.
    read: Arc<dyn ReadSource>,

    /// If the system is enabled.
    enabled: bool,

    /// If true, don't require an actual `.git` directory to apply
    /// gitignore / `.git/info/exclude` rules (mirrors
    /// `ignore::WalkBuilder::require_git(false)`). Does not affect
    /// `custom_ignorefiles`, which are always honored when `enabled`.
    no_require_git: bool,

    /// Extra filenames to treat as gitignore-syntax ignore files, e.g.
    /// `.rusticignore`, applied at every directory level, same as
    /// `.gitignore`.
    custom_ignorefiles: Vec<String>,

    /// path -> compiled matcher; per-directory, built lazily as the walker
    /// descends and consulted top-down (see `is_ok`). `None` means the
    /// directory was checked but had nothing to load.
    layers: DashMap<PathBuf, Option<Gitignore>>,

    /// Lazily-resolved, cached repo root (nearest ancestor containing
    /// `.git`), or `None` if no repo was found. Resolved on first
    /// `load_dir` call and reused for the rest of the walk, since the
    /// repo root can't change mid-walk for a single lister instance.
    repo_root: OnceLock<Option<PathBuf>>,
}

impl GitIgnoreFilter {
    pub fn new(read: Arc<dyn ReadSource>, opts: &FilterOptions) -> Self {
        Self {
            read,
            enabled: opts.git_ignore,
            no_require_git: opts.no_require_git,
            custom_ignorefiles: opts.custom_ignorefiles.clone(),
            layers: DashMap::new(),
            repo_root: OnceLock::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Walks upward from `start` looking for a directory containing
    /// `.git` (file or directory — covers both regular repos and
    /// worktrees, where `.git` is a file pointing elsewhere). Terminates
    /// naturally once `Path::parent()` runs out (e.g. at a virtualized
    /// `"/"` root).
    fn find_repo_root(&self, start: &Path) -> io::Result<Option<PathBuf>> {
        let mut dir = Some(start.to_path_buf());
        while let Some(d) = dir {
            if self.read.exists(&d.join(".git"))? {
                return Ok(Some(d));
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        Ok(None)
    }

    /// Resolves (and caches) the repo root for this walk, computed once
    /// from the first directory passed to `load_dir` (normally the walk
    /// root itself).
    fn repo_root(&self, start: &Path) -> io::Result<Option<PathBuf>> {
        if let Some(cached) = self.repo_root.get() {
            return Ok(cached.clone());
        }
        let found = self.find_repo_root(start)?;
        // Another caller may have raced us to compute this; both would
        // arrive at the same answer for the same `start`, so a failed
        // `set` here is harmless.
        let _ = self.repo_root.set(found.clone());
        Ok(found)
    }

    /// Ensures the ignore-file layer for `dir` is loaded (reading
    /// `.gitignore` and `custom_ignorefiles` present directly in `dir`,
    /// plus `.git/info/exclude` when `dir` is the repo root), caching the
    /// compiled matcher (or `None` if there was nothing to load) for
    /// reuse by descendants.
    ///
    /// Call this once per directory, before filtering its children.
    ///
    /// # Errors
    ///
    /// Returns an error if an ignore file could not be read, or if its
    /// contents could not be parsed as gitignore syntax.
    pub fn load_dir(&self, dir: &Path) -> io::Result<()> {
        if self.layers.contains_key(dir) {
            return Ok(());
        }

        let repo_root = self.repo_root(dir)?;
        let honor_git_files = self.no_require_git || repo_root.is_some();

        let mut builder = GitignoreBuilder::new(dir);
        let mut had_any = false;

        if honor_git_files {
            if let Some(contents) = self.read_small_file(&dir.join(".gitignore"))? {
                had_any |= self.add_lines(&mut builder, &contents, ".gitignore", dir)?;
            }

            // `.git/info/exclude` behaves like an extra top-level
            // .gitignore anchored at the repo root, so only read it once,
            // when we're loading the repo root's own layer.
            if let Some(root) = &repo_root {
                if root == dir {
                    let exclude_path = root.join(".git").join("info").join("exclude");
                    if let Some(contents) = self.read_small_file(&exclude_path)? {
                        had_any |= self.add_lines(&mut builder, &contents, "info/exclude", root)?;
                    }
                }
            }
        }

        for filename in &self.custom_ignorefiles {
            let file_path = dir.join(filename);
            if let Some(contents) = self.read_small_file(&file_path)? {
                had_any |= self.add_lines(&mut builder, &contents, filename, dir)?;
            }
        }

        let compiled = if had_any {
            Some(builder.build().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "failed to build gitignore matcher for directory `{}`: {err}",
                        dir.display()
                    ),
                )
            })?)
        } else {
            None
        };

        let _ = self.layers.insert(dir.to_path_buf(), compiled);
        Ok(())
    }

    /// Adds every non-empty line of `contents` (from a file logically
    /// named `label`, used only for error messages) to `builder`,
    /// anchored at `anchor`. Returns `true` if at least one line was
    /// added.
    ///
    /// `anchor` matters: ordinary `.gitignore`/custom ignore files are
    /// anchored at the directory they live in, but `.git/info/exclude`
    /// must be anchored at the repo root even though the file itself
    /// physically lives under `<root>/.git/info/`.
    fn add_lines(
        &self,
        builder: &mut GitignoreBuilder,
        contents: &str,
        label: &str,
        anchor: &Path,
    ) -> io::Result<bool> {
        let mut added = false;
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            builder
                .add_line(Some(anchor.to_path_buf()), line)
                .map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("failed to parse ignore line `{line}` from `{label}`: {err}"),
                    )
                })?;
            added = true;
        }
        Ok(added)
    }

    /// Reads a small file's full contents via the backend, returning `None`
    /// if it doesn't exist. Ignore files are assumed small (KBs), so this
    /// reads fully into memory rather than streaming.
    fn read_small_file(&self, path: &Path) -> io::Result<Option<String>> {
        match self.read.stat(path) {
            Ok(None) => return Ok(None),
            Ok(Some(_)) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!("failed to stat ignore file `{}`: {err}", path.display()),
                ));
            }
        }

        let mut handle = match self.read.open_read(path) {
            Ok(h) => h,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!(
                        "failed to open ignore file `{}` for reading: {err}",
                        path.display()
                    ),
                ));
            }
        };

        let mut buf = String::new();
        handle.read_to_string(&mut buf).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to read ignore file `{}`: {err}", path.display()),
            )
        })?;
        let _ = handle.close();
        Ok(Some(buf))
    }

    /// Returns `true` if `file` should be kept, consulting every loaded
    /// ancestor layer from the walk root down to `file`'s parent
    /// directory (deeper `.gitignore` files take precedence, matching
    /// git's own semantics).
    ///
    /// `ancestors` must be the list of directories from root to `file`'s
    /// parent, in top-down order, each already `load_dir`-ed by the
    /// caller.
    pub fn is_ok(&self, file: &File, ancestors: &[PathBuf]) -> bool {
        if !self.enabled {
            return true;
        }

        let is_dir = file.is_dir();
        // Walk shallowest to deepest so the most specific (deepest)
        // .gitignore wins, mirroring git's precedence rules.
        for dir in ancestors {
            if let Some(layer) = self.layers.get(dir) {
                if let Some(gi) = layer.value() {
                    match gi.matched(file.path(), is_dir) {
                        Match::Ignore(_) => return false,
                        Match::Whitelist(_) => return true,
                        Match::None => {}
                    }
                }
            }
        }
        true
    }
}
