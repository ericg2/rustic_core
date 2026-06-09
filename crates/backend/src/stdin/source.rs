use std::io::{stdin, Stdin};
use std::iter::{once, Once};
use std::path::{Path, PathBuf};
use derive_setters::Setters;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use rustic_core::{ErrorKind, ReadSource, ReadSourceBuilder, ReadSourceEntry, RusticError, RusticResult};

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Debug, Setters, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct StdinSource {
    /// The path of the stdin entry.
    #[setters(skip)]
    output: PathBuf
}

impl StdinSource {
    /// Creates a new [`StdinSource`] with the path to output to.
    pub fn new(output: impl AsRef<Path>) -> Self {
        Self {
            output: output.as_ref().to_path_buf()
        }
    }
}

impl ReadSourceBuilder for StdinSource {
    type Reader = StdinReader;

    fn get_reader(&self) -> RusticResult<Self::Reader> {
        Ok(StdinReader::new(&self))
    }
}

/// The `StdinReader` is a `ReadSource` for stdin.
#[derive(Debug, Clone)]
pub struct StdinReader {
    /// The path of the stdin entry.
    config: StdinSource,
}

impl StdinReader {
    /// Creates a new `StdinSource`.
    pub(crate) fn new(config: &StdinSource) -> Self {
        Self { config: config.to_owned() }
    }
}

impl ReadSource for StdinReader {
    /// The open type.
    type Open = Stdin;
    /// The iterator type.
    type Iter = Once<RusticResult<ReadSourceEntry<Stdin>>>;

    /// Returns the size of the source.
    fn size(&self) -> RusticResult<Option<u64>> {
        Ok(None)
    }

    /// Returns an iterator over the source.
    fn entries(&self) -> Self::Iter {
        let open = Some(stdin());
        once(
            ReadSourceEntry::from_path(self.config.output.clone(), open).map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Backend,
                    "Failed to create ReadSourceEntry from Stdin",
                    err,
                )
            }),
        )
    }

    fn paths(&self) -> Vec<PathBuf> {
        vec![self.config.output.clone()]
    }
}
