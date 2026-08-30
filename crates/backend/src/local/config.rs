use crate::local::backend::LocalSource;
use derive_setters::Setters;
use rustic_core::{BackendConfig, ErrorKind, RusticError, RusticResult, WriteBackend, WriteSource};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::repo::RepoAdapter;

#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
/// A local [`Repository`].
pub struct LocalConfig {
    /// The base path of the backend.
    pub path: Option<PathBuf>,
}

impl LocalConfig {
    /// Creates a new [`LocalRepo`] with the given [`Path`].
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_path_buf()),
        }
    }
}

impl BackendConfig for LocalConfig {
    type Output = RepoAdapter<LocalSource>;

    fn from_iter<K, V, I>(path: impl AsRef<str>, _dict: I) -> Self
    where
        I: IntoIterator<Item=(K, V)>,
        K: Into<String>,
        V: Into<String>
    {
        Self {
            path: Some(PathBuf::from(path.as_ref())),
        }
    }

    fn get_path(&self) -> Option<String> {
        self.path.clone().map(|x| x.to_string_lossy().to_string())
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self);
        let _ = ret.remove("path");
        ret
    }

    fn get_repo(&self) -> RusticResult<Self::Output> {
        let src = LocalSource::from_config(&self)?;
        Ok(RepoAdapter::new(src))
    }
}
