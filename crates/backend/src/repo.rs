use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
};

use aho_corasick::AhoCorasick;
use bytes::Bytes;
use log::{debug, error, trace, warn};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use typed_path::UnixPathBuf;

use rustic_core::{
    ALL_FILE_TYPES, CommandInput, ErrorKind, FileType, Id, ListAdapter, ListOptions, Metadata,
    ReadBackend, ReadSource, RepositoryOptions, RusticError, RusticResult, WriteBackend,
    WriteSource,
};

// ---------------------------------------------------------------------
// Shared, backend-agnostic helpers
// ---------------------------------------------------------------------
//
// These used to live as private methods on `RepoAdapter`. They're now free
// functions so that both `RepoAdapter` (which turns a `WriteSource` into a
// backend) and `CommandBackend<B>` (which wraps *any* existing backend with
// post-create/post-delete command hooks) can share the exact same logic.

/// Compute the on-disk (or on-backend) path for a given file type / id,
/// following rustic's standard repository layout.
fn repo_path(tpe: FileType, id: &Id) -> PathBuf {
    let hex_id = id.to_hex();
    match tpe {
        FileType::Config => UnixPathBuf::from("config"),
        FileType::Pack => UnixPathBuf::from("data").join(&hex_id[..2]).join(&*hex_id),
        _ => UnixPathBuf::from(tpe.dirname()).join(&*hex_id),
    }
    .to_string()
    .into()
}

// ---------------------------------------------------------------------
// RepoAdapter: turns a `WriteSource` into a `ReadBackend` / `WriteBackend`
// ---------------------------------------------------------------------
//
// This is now a "pure" adapter: it no longer knows anything about
// post-create/post-delete commands. That behavior lives in `CommandBackend`
// below, which can wrap this adapter (or any other backend).

pub struct RepoAdapter<S> {
    /// The [`WriteSource`] for this adapter.
    be: S,
}

impl<S> RepoAdapter<S> {
    pub fn new(be: S) -> Self {
        Self { be }
    }
}

impl<S: WriteSource> ReadBackend for RepoAdapter<S> {
    fn location(&self) -> String {
        self.be.location()
    }

    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        fn length(entry: &Metadata, file_name: &str, tpe: FileType) -> Option<u32> {
            let length = entry.size;
            length
                .try_into()
                .inspect_err(|err| {
                    error!(
                        "Failed to convert file length {length} of {file_name} to u32 while listing {tpe}: {err}"
                    );
                })
                .ok()
        }

        trace!("listing tpe: {tpe:?}");

        if tpe == FileType::Config {
            return match self.be.stat(Path::new("config")) {
                Ok(Some(meta)) => Ok(vec![(
                    Id::default(),
                    length(&meta, "config", tpe).unwrap_or_default(),
                )]),
                Ok(None) => Ok(Vec::new()),
                Err(err) => Err(RusticError::with_source(
                    ErrorKind::Backend,
                    "Getting Metadata of type `{type}` failed in the backend. Please check if `{type}` exists.",
                    err,
                )
                    .attach_context("type", tpe.to_string())),
            };
        }

        let lister = ListAdapter::new(&self.be, tpe.dirname()).map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Listing failed for `{type}`", err)
                .attach_context("type", tpe.to_string())
        })?;

        Ok(lister
            .filter_map(|r| {
                let entry = r
                    .inspect_err(|err| error!("error listing {tpe}: {err}"))
                    .ok()?;

                if !entry.is_file() {
                    return None;
                }

                let name = entry.name();
                let id = Id::parse_some(name, tpe)?;
                let length = length(entry.metadata(), name, tpe)?;

                Some((id, length))
            })
            .collect())
    }

    fn list(&self, tpe: FileType) -> RusticResult<Vec<Id>> {
        trace!("listing tpe: {tpe:?}");

        if tpe == FileType::Config {
            return Ok(
                if self.be.exists(Path::new("config")).map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::Backend,
                        "Path `config` does not exist.",
                        err,
                    )
                    .ask_report()
                })? {
                    vec![Id::default()]
                } else {
                    Vec::new()
                },
            );
        }

        let lister = ListAdapter::new(&self.be, tpe.dirname()).map_err(|err| {
            RusticError::with_source(ErrorKind::Backend, "Listing failed for `{type}`", err)
                .attach_context("type", tpe.to_string())
        })?;

        Ok(lister
            .filter_map(|r| {
                let entry = r
                    .inspect_err(|err| error!("error listing {tpe}: {err}"))
                    .ok()?;

                if !entry.is_file() {
                    return None;
                }

                Id::parse_some(entry.name(), tpe)
            })
            .collect())
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        trace!("reading tpe: {tpe:?}, id: {id}");
        let path = repo_path(tpe, id);
        let mut buf = Vec::new();

        self.be
            .open_read(&path)
            .and_then(|mut x| {
                x.read_to_end(&mut buf)?;
                x.close()
            })
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Reading file `{path}` failed in the backend. Please check if the given path is correct.",
                    err,
                )
                    .attach_context("path", path.display().to_string())
                    .attach_context("type", tpe.to_string())
                    .attach_context("id", id.to_string())
            })?;

        Ok(buf.into())
    }

    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        _cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes> {
        trace!("reading tpe: {tpe:?}, id: {id}, offset: {offset}, length: {length}");
        let path = repo_path(tpe, id);
        let mut buf = vec![0; length as usize];

        self.be
            .open_read(&path)
            .and_then(|mut x| {
                x.seek(SeekFrom::Start(offset as u64))?;
                x.read_exact(&mut buf)?;
                x.close()
            })
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Reading file `{path}` failed in the backend. Please check if the given path is correct.",
                    err,
                )
                    .attach_context("path", path.display().to_string())
                    .attach_context("type", tpe.to_string())
                    .attach_context("id", id.to_string())
                    .attach_context("offset", offset.to_string())
                    .attach_context("length", length.to_string())
            })?;

        Ok(buf.into())
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        repo_path(tpe, id).to_string_lossy().into_owned()
    }
}

impl<S: WriteSource> WriteBackend for RepoAdapter<S> {
    fn create(&self) -> RusticResult<()> {
        trace!("creating repo at {:?}", self.location());
        for tpe in ALL_FILE_TYPES {
            let path = tpe.dirname().to_string() + "/";
            self.be
                .create_dir_all(Path::new(&path))
                .map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::Backend,
                        "Creating directory `{path}` failed in the backend `{location}`. Please check if the given path is correct.",
                        err,
                    )
                        .attach_context("path", path)
                        .attach_context("location", self.location())
                        .attach_context("type", tpe.to_string())
                })?;
        }
        // creating 256 dirs can be slow on remote backends, hence we parallelize it.
        (0u8..=255).into_par_iter().try_for_each(|i| {
            let path: PathBuf = UnixPathBuf::from("data")
                .join(hex::encode([i]))
                .to_string()
                .into();

            self.be.create_dir_all(&path).map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Creating directory `{path}` failed in the backend `{location}`. Please check if the given path is correct.",
                    err,
                )
                    .attach_context("path", path.display().to_string())
                    .attach_context("location", self.location())
            })
        })?;

        Ok(())
    }

    fn write_bytes(
        &self,
        tpe: FileType,
        id: &Id,
        _cacheable: bool,
        buf: Bytes,
    ) -> RusticResult<()> {
        trace!("writing tpe: {:?}, id: {}", &tpe, &id);
        let filename = repo_path(tpe, id);
        self.be.write_all(&filename, buf).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Writing file `{path}` failed in the backend. Please check if the given path is correct.",
                err,
            )
                .attach_context("path", filename.display().to_string())
                .attach_context("type", tpe.to_string())
                .attach_context("id", id.to_string())
        })?;

        Ok(())
    }

    fn remove(&self, tpe: FileType, id: &Id, _cacheable: bool) -> RusticResult<()> {
        trace!("removing tpe: {:?}, id: {}", &tpe, &id);
        let filename = repo_path(tpe, id);
        self.be.remove_file(&filename).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Deleting file `{path}` failed in the backend. Please check if the given path is correct.",
                err,
            )
                .attach_context("path", filename.display().to_string())
                .attach_context("type", tpe.to_string())
                .attach_context("id", id.to_string())
        })?;

        Ok(())
    }
}
