use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use aho_corasick::AhoCorasick;
use bytes::Bytes;
use log::{debug, error, trace, warn};
use typed_path::UnixPathBuf;

use rustic_core::{
    ALL_FILE_TYPES, CommandInput, ErrorKind, FileType, Id, Metadata, ReadBackend, ReadSource,
    RepositoryOptions, RusticError, RusticResult, WriteBackend,
    backend::{ListOptions, WriteSource, list::ListAdapter},
};

pub struct RepoAdapter {
    /// The [`WriteSource`] for this adapter.
    be: Arc<dyn WriteSource>,
    /// All [`RepositoryOptions`] for this adapter.
    config: RepositoryOptions,
}

impl RepoAdapter {
    #[allow(clippy::unused_self)]
    fn path(&self, tpe: FileType, id: &Id) -> PathBuf {
        let hex_id = id.to_hex();

        match tpe {
            FileType::Config => UnixPathBuf::from("config"),
            FileType::Pack => UnixPathBuf::from("data").join(&hex_id[..2]).join(&hex_id),
            _ => UnixPathBuf::from(tpe.dirname()).join(&hex_id),
        }
        .into()
    }

    /// Call the given command.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `filename` - The path to the file.
    /// * `command` - The command to call.
    ///
    /// # Errors
    ///
    /// * If the patterns could not be compiled.
    /// * If the command could not be parsed.
    /// * If the command could not be executed.
    /// * If the command was not successful.
    ///
    /// # Notes
    ///
    /// The following placeholders are supported:
    /// * `%file` - The path to the file.
    /// * `%type` - The type of the file.
    /// * `%id` - The id of the file.
    fn call_command(tpe: FileType, id: &Id, filename: &Path, command: &String) -> RusticResult<()> {
        let id = id.to_hex();
        let patterns = &["%file", "%type", "%id"];
        let ac = AhoCorasick::new(patterns).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Internal,
                "Experienced an error building AhoCorasick automaton for command replacement.",
                err,
            )
            .ask_report()
        })?;

        let replace_with = &[filename.to_str().unwrap(), tpe.dirname(), id.as_str()];
        let actual_command = ac.replace_all(command, replace_with);
        debug!("calling {actual_command}...");

        let command: CommandInput = actual_command.parse().map_err(|err| {
            RusticError::with_source(
                ErrorKind::Internal,
                "Failed to parse command input: `{command}` is not a valid command.",
                err,
            )
            .attach_context("command", actual_command)
            .attach_context("replacement", replace_with.join(", "))
            .ask_report()
        })?;

        let status = Command::new(command.command())
            .args(command.args())
            .status()
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::ExternalCommand,
                    "Failed to execute `{command}`. Please check the command and try again.",
                    err,
                )
                .attach_context("command", command.to_string())
            })?;

        if !status.success() {
            return Err(RusticError::new(
                ErrorKind::ExternalCommand,
                "Command was not successful: `{command}` failed with status `{status}`.",
            )
            .attach_context("command", command.to_string())
            .attach_context("file_name", replace_with[0])
            .attach_context("file_type", replace_with[1])
            .attach_context("id", replace_with[2])
            .attach_context("status", status.to_string()));
        }
        Ok(())
    }
}

impl ReadBackend for RepoAdapter {
    fn location(&self) -> String {
        self.be.location()
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

        let lister = self
            .0
            .lister(
                Path::new(tpe.dirname()),
                ListOptions {
                    recursive: true,
                    ..Default::default()
                },
            )
            .map_err(|err| {
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

    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        fn length(entry: &Metadata, file_name: &str, tpe: FileType) -> Option<u32> {
            entry.content_length()
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
                Err(err) => Err(
                    RusticError::with_source(
                        ErrorKind::Backend,
                        "Getting Metadata of type `{type}` failed in the backend. Please check if `{type}` exists.",
                        err,
                    )
                    .attach_context("type", tpe.to_string()),
                ),
            };
        }

        let lister = ListAdapter::new(
            self.be.clone(),
            tpe.dirname(),
            ListOptions::default().recursive(true),
        )
        .map_err(|err| {
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

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        trace!("reading tpe: {tpe:?}, id: {id}");
        let path = self.path(tpe, id);
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
                .attach_context("path", path)
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
        let path = self.path(tpe, id);
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
                .attach_context("path", path)
                .attach_context("type", tpe.to_string())
                .attach_context("id", id.to_string())
                .attach_context("offset", offset.to_string())
                .attach_context("length", length.to_string())
            })?;

        Ok(buf.into())
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        self.path(tpe, id).to_string_lossy().into_owned()
    }
}

impl WriteBackend for RepoAdapter {
    fn create(&self) -> RusticResult<()> {
        trace!("creating repo at {:?}", self.location());
        for tpe in ALL_FILE_TYPES {
            let path = tpe.dirname().to_string() + "/";
            self.be
                      .create_dir_all(Path::new(&path))
                      .map_err(|err|
                          RusticError::with_source(
                              ErrorKind::Backend,
                              "Creating directory `{path}` failed in the backend `{location}`. Please check if the given path is correct.",
                              err,
                          )
                              .attach_context("path", path)
                              .attach_context("location", self.location())
                              .attach_context("type", tpe.to_string())
                      )?;
        }
        // creating 256 dirs can be slow on remote backends, hence we parallelize it.
        (0u8..=255)
                  .into_par_iter()
                  .try_for_each(|i| {
                      let path = UnixPathBuf::from("data")
                          .join(hex::encode([i])).into();

                      self.be.create_dir_all(&path).map_err(|err|
                          RusticError::with_source(
                              ErrorKind::Backend,
                              "Creating directory `{path}` failed in the backend `{location}`. Please check if the given path is correct.",
                              err,
                          )
                              .attach_context("path", path)
                              .attach_context("location", self.location())
                      )
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
        let filename = self.path(tpe, id);
        _ = self.be.write_all(&filename, buf).map_err(|err| {
                  RusticError::with_source(
                      ErrorKind::Backend,
                      "Writing file `{path}` failed in the backend. Please check if the given path is correct.",
                      err,
                  )
                      .attach_context("path", filename)
                      .attach_context("type", tpe.to_string())
                      .attach_context("id", id.to_string())
              })?;

        if let Some(command) = &self.config.post_create_command
            && let Err(err) = Self::call_command(tpe, id, &filename, command)
        {
            warn!("post-create: {}", err.display_log());
        }

        Ok(())
    }

    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()> {
        trace!("removing tpe: {:?}, id: {}", &tpe, &id);
        let filename = self.path(tpe, id);
        self.be.remove_file(&filename).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Backend,
                "Deleting file `{path}` failed in the backend. Please check if the given path is correct.",
                err,
            )
                .attach_context("path", filename)
                .attach_context("type", tpe.to_string())
                .attach_context("id", id.to_string())
        })?;

        if let Some(command) = &self.config.post_delete_command
            && let Err(err) = Self::call_command(tpe, id, &filename, command)
        {
            warn!("post-delete: {}", err.display_log());
        }
        Ok(())
    }
}
