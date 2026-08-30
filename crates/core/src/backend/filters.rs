use bytesize::ByteSize;
use derive_setters::Setters;
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;

use crate::{BlockdevOption, DevIdOption, TimeOption, XattrOption};

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(serde::Deserialize, serde::Serialize, Default, Clone, Debug, Setters, PartialEq, Eq)]
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
/// Gitignore-style matching against an arbitrary [`ReadSource`] backend,
/// mirroring `ignore::WalkBuilder`'s handling of `.gitignore`,
/// `.git/info/exclude`, and custom ignore files.
pub struct GitignoreLayers<'a, R: ReadSource> {
    be: &'a R,
    enabled: bool,
    no_require_git: bool,
    custom_ignore_files: Vec<String>,
    layers: HashMap<PathBuf, Option<Gitignore>>,
    repo_root: Option<Option<PathBuf>>,
}

impl<'a, R: ReadSource> GitignoreLayers<'a, R> {
    pub fn new(be: &'a R, opts: &FilterOptions) -> Self {
        Self {
            be,
            enabled: opts.git_ignore,
            no_require_git: opts.no_require_git,
            custom_ignore_files: opts.custom_ignorefiles.clone(),
            layers: HashMap::new(),
            repo_root: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn find_repo_root(&self, start: &Path) -> io::Result<Option<PathBuf>> {
        for dir in start.ancestors() {
            if self.be.exists(&dir.join(".git"))? {
                return Ok(Some(dir.to_path_buf()));
            }
        }
        Ok(None)
    }

    fn repo_root(&mut self, start: &Path) -> io::Result<Option<PathBuf>> {
        if let Some(cached) = &self.repo_root {
            return Ok(cached.clone());
        }
        let found = self.find_repo_root(start)?;
        self.repo_root = Some(found.clone());
        Ok(found)
    }

    /// Loads and caches the gitignore matcher for `dir`. Call once per
    /// directory before filtering its children.
    pub fn load_dir(&mut self, dir: &Path) -> io::Result<()> {
        if self.layers.contains_key(dir) {
            return Ok(());
        }

        let repo_root = self.repo_root(dir)?;
        let honor_git_files = self.no_require_git || repo_root.is_some();
        let mut builder = GitignoreBuilder::new(dir);
        let mut had_any = false;

        if honor_git_files {
            if let Some(contents) = self.read_small_file(&dir.join(".gitignore"))? {
                had_any |= add_lines(&mut builder, &contents, ".gitignore", dir)?;
            }

            if let Some(root) = repo_root.as_deref().filter(|root| *root == dir) {
                let exclude_path = root.join(".git").join("info").join("exclude");
                if let Some(contents) = self.read_small_file(&exclude_path)? {
                    had_any |= add_lines(&mut builder, &contents, "info/exclude", root)?;
                }
            }
        }

        for filename in &self.custom_ignore_files {
            if let Some(contents) = self.read_small_file(&dir.join(filename))? {
                had_any |= add_lines(&mut builder, &contents, filename, dir)?;
            }
        }

        let compiled = had_any
            .then(|| builder.build())
            .transpose()
            .map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "failed to build gitignore matcher for `{}`: {err}",
                        dir.display()
                    ),
                )
            })?;

        self.layers.insert(dir.to_path_buf(), compiled);
        Ok(())
    }

    fn read_small_file(&self, path: &Path) -> io::Result<Option<String>> {
        let mut handle = match self.be.open_read(path) {
            Ok(h) => h,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!("failed to open `{}`: {err}", path.display()),
                ));
            }
        };

        let mut buf = String::new();
        handle.read_to_string(&mut buf).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to read `{}`: {err}", path.display()),
            )
        })?;
        let _ = handle.close();
        Ok(Some(buf))
    }

    /// Whether `path` survives gitignore filtering, given its loaded
    /// ancestor directories (root to parent, top-down).
    pub fn is_ok(&self, path: &Path, is_dir: bool, ancestors: &[PathBuf]) -> bool {
        if !self.enabled {
            return true;
        }

        for dir in ancestors {
            let Some(Some(gi)) = self.layers.get(dir) else {
                continue;
            };
            match gi.matched(path, is_dir) {
                Match::Ignore(_) => return false,
                Match::Whitelist(_) => return true,
                Match::None => {}
            }
        }
        true
    }
}

fn add_lines(
    builder: &mut GitignoreBuilder,
    contents: &str,
    label: &str,
    anchor: &Path,
) -> io::Result<bool> {
    let mut added = false;
    for line in contents.lines().filter(|l| !l.trim().is_empty()) {
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
