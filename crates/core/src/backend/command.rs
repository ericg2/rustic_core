use std::path::{Path, PathBuf};
use std::process::Command;
use aho_corasick::AhoCorasick;
use bytes::Bytes;
use log::{debug, warn};
use typed_path::UnixPathBuf;
use crate::{CommandInput, ErrorKind, FileType, Id, ReadBackend, RepositoryOptions, RusticError, RusticResult, WriteBackend};


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

// ---------------------------------------------------------------------
// CommandBackend: a generic 2nd-stage wrapper adding post-create /
// post-delete command hooks to *any* backend.
// ---------------------------------------------------------------------
//
// This is the piece you asked for: it takes any `B: ReadBackend` /
// `B: WriteBackend` — whether that's a `RepoAdapter<S>`, or a totally
// custom backend you've implemented yourself — and layers the
// `post_create_command` / `post_delete_command` hook logic on top,
// without duplicating the command-invocation logic anywhere.
//
// Usage:
//
// ```ignore
// // Wrap the standard adapter:
// let backend = CommandBackend::new(RepoAdapter::new(my_write_source), options);
//
// // Or wrap your own custom backend:
// let backend = CommandBackend::new(my_custom_backend, options);
// ```
pub struct CommandBackend<B> {
    /// The wrapped backend (read-only, write-only, or both).
    be: B,
    /// All [`RepositoryOptions`] for this wrapper (used for the command hooks).
    config: RepositoryOptions,
}

impl<B> CommandBackend<B> {
    pub fn new(be: B, config: RepositoryOptions) -> Self {
        Self { be, config }
    }
}

impl<B: ReadBackend> ReadBackend for CommandBackend<B> {
    fn location(&self) -> String {
        self.be.location()
    }

    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        self.be.list_with_size(tpe)
    }

    fn list(&self, tpe: FileType) -> RusticResult<Vec<Id>> {
        self.be.list(tpe)
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        self.be.read_full(tpe, id)
    }

    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes> {
        self.be.read_partial(tpe, id, cacheable, offset, length)
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        self.be.warmup_path(tpe, id)
    }
}

impl<B: WriteBackend> WriteBackend for CommandBackend<B> {
    fn create(&self) -> RusticResult<()> {
        self.be.create()
    }

    fn write_bytes(&self, tpe: FileType, id: &Id, cacheable: bool, buf: Bytes) -> RusticResult<()> {
        self.be.write_bytes(tpe, id, cacheable, buf)?;

        if let Some(command) = &self.config.post_create_command {
            let filename = repo_path(tpe, id);
            if let Err(err) = call_command(tpe, id, &filename, command) {
                warn!("post-create: {}", err.display_log());
            }
        }

        Ok(())
    }

    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()> {
        self.be.remove(tpe, id, cacheable)?;

        if let Some(command) = &self.config.post_delete_command {
            let filename = repo_path(tpe, id);
            if let Err(err) = call_command(tpe, id, &filename, command) {
                warn!("post-delete: {}", err.display_log());
            }
        }

        Ok(())
    }
}
