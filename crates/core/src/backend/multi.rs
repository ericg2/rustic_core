//! A [`ReadSource`] that dispatches requests to one of several backends.
//!
//! [`MultiSource`] allows a single backup to span multiple source roots,
//! potentially backed by completely different [`ReadSource`] implementations.
//!
//! For example, a backup could combine:
//!
//! ```text
//! /home  -> local filesystem
//! /repo  -> repository/archive backend
//! /mnt   -> remote filesystem
//! ```
//!
//! The caller supplies each root together with the [`ReadSource`] responsible
//! for it. `MultiSource` then resolves every operation to the appropriate
//! source using the configured root paths.
//!
//! `MultiSource` does not own the source implementations. Instead, it borrows
//! them through [`&dyn ReadSource`]. This avoids requiring `Arc` solely for
//! sharing a backend between multiple roots.
//!
//! Downstream code such as [`ListAdapter`](crate::ListAdapter) and
//! [`Archiver`](crate::archiver::Archiver) sees only one [`ReadSource`]
//! implementation and therefore does not need to know that multiple backends
//! are involved.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::backend::{Metadata, Node, ReadHandle, ReadSource};

/// A source root paired with the backend responsible for reading it.
///
/// The backend is borrowed rather than owned. This allows multiple
/// [`SourceRoot`] values to refer to the same [`ReadSource`] without requiring
/// `Arc`.
///
/// `path` identifies the root as it appears in the combined source namespace.
/// When [`MultiSource`] receives a path underneath this root, it dispatches
/// the operation to `be`.
#[derive(Clone)]
pub struct SourceRoot<'a> {
    /// Backend responsible for this root.
    pub be: &'a dyn ReadSource,

    /// Root path in the combined source namespace.
    pub path: PathBuf,
}

impl<'a> SourceRoot<'a> {
    /// Creates a source root backed by `be`.
    ///
    /// `path` is converted to a [`PathBuf`] and becomes the root of this
    /// source in the combined [`MultiSource`] namespace.
    pub fn new(be: &'a dyn ReadSource, path: impl AsRef<Path>) -> Self {
        Self {
            be,
            path: path.as_ref().to_path_buf(),
        }
    }
}

/// A [`ReadSource`] that dispatches operations to multiple backends.
///
/// Each [`SourceRoot`] associates a namespace path with a borrowed
/// [`ReadSource`]. When an operation is performed, `MultiSource` finds the
/// configured root containing the requested path and forwards the operation
/// to that backend.
///
/// Roots are ordered from longest to shortest path during construction. This
/// gives nested roots precedence over their parents. For example:
///
/// ```text
/// /a
/// /a/b
/// ```
///
/// A request for `/a/b/file.txt` is therefore handled by the `/a/b` source,
/// not the `/a` source.
///
/// `MultiSource` borrows all of its backends for its lifetime.
pub struct MultiSource<'a> {
    /// Configured source roots, ordered from longest to shortest path.
    roots: Vec<SourceRoot<'a>>,
}

impl<'a> MultiSource<'a> {
    /// Creates a [`MultiSource`] from the supplied source roots.
    ///
    /// Roots are sorted by descending path length so that the most specific
    /// matching root always takes precedence.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] when `roots` is empty.
    pub fn new(mut roots: Vec<SourceRoot<'a>>) -> io::Result<Self> {
        if roots.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MultiSource: at least one source root is required",
            ));
        }

        roots.sort_by_key(|root| {
            std::cmp::Reverse(root.path.as_os_str().len())
        });

        Ok(Self { roots })
    }

    /// Returns the configured root paths.
    ///
    /// Paths are returned in the same longest-first order used when resolving
    /// requests. Consequently, this order may differ from the order in which
    /// the roots were originally supplied.
    pub fn root_paths(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .map(|root| root.path.clone())
            .collect()
    }

    /// Resolves `path` to the backend responsible for it.
    ///
    /// Resolution uses [`Path::starts_with`], with roots checked from longest
    /// to shortest. This ensures that a more-specific nested root wins over a
    /// broader root.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::NotFound`] if `path` does not belong to any
    /// configured root.
    fn resolve(&self, path: &Path) -> io::Result<&dyn ReadSource> {
        self.roots
            .iter()
            .find(|root| path.starts_with(&root.path))
            .map(|root| root.be)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "`{}` is not under any configured source root",
                        path.display()
                    ),
                )
            })
    }
}

impl<'a> ReadSource for MultiSource<'a> {
    /// Returns a human-readable description of all configured roots and their
    /// backing sources.
    ///
    /// Each entry has the form:
    ///
    /// ```text
    /// <root>@<source-location>
    /// ```
    ///
    /// Multiple entries are separated by commas.
    fn location(&self) -> String {
        self.roots
            .iter()
            .map(|root| {
                format!(
                    "{}@{}",
                    root.path.display(),
                    root.be.location()
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Opens `path` for reading using the backend responsible for its root.
    ///
    /// The path is passed unchanged to the selected backend, preserving the
    /// existing path semantics of the individual [`ReadSource`] implementations.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadHandle>> {
        self.resolve(path)?.open_read(path)
    }

    /// Lists the directory at `path` using the backend responsible for its
    /// root.
    ///
    /// The returned iterator is produced directly by the selected backend.
    fn readdir(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<Node>> + Send>> {
        self.resolve(path)?.readdir(path)
    }

    /// Returns metadata for `path` from the backend responsible for its root.
    fn stat(&self, path: &Path) -> io::Result<Option<Metadata>> {
        self.resolve(path)?.stat(path)
    }

    /// Checks whether `path` exists using the backend responsible for its root.
    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.resolve(path)?.exists(path)
    }
}