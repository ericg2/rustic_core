use crate::local::backend::LocalBackend;
use derive_setters::Setters;
use rustic_core::{ErrorKind, RepositoryConfig, RusticError, RusticResult, WriteBackend};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
/// A local [`Repository`].
pub struct LocalConfig {
    /// The base path of the backend.
    pub path: Option<PathBuf>,
    /// The command to call after a file was created.
    pub post_create_command: Option<String>,
    /// The command to call after a file was deleted.
    pub post_delete_command: Option<String>,
}

impl LocalConfig {
    /// Creates a new [`LocalConfig`] with the given [`Path`].
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_path_buf()),
            post_create_command: None,
            post_delete_command: None,
        }
    }

    /// Creates an [`LocalConfig`] from an iterator.
    ///
    /// # Important
    /// This does not guarantee the [`LocalConfig`] is initialized correctly. Due to the
    /// nature of dynamic types - this feature is only a convenience. All invalid fields will
    /// be skipped, and will not return an error during this process.
    pub fn from_iter<K, V, I>(path: impl AsRef<str>, dict: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let map: HashMap<String, String> = dict
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();

        Self {
            path: Some(PathBuf::from(path.as_ref())),
            post_create_command: map.get("post-create-command").cloned(),
            post_delete_command: map.get("post-delete-command").cloned(),
        }
    }
}

impl RepositoryConfig for LocalConfig {
    fn get_path(&self) -> Option<String> {
        self.path.clone().map(|x| x.to_string_lossy().to_string())
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self);
        let _ = ret.remove("path");
        ret
    }

    fn get_repo(&self) -> RusticResult<Arc<dyn WriteBackend>> {
        // Make sure the fields are correctly filled.
        let config = self.path.as_ref().ok_or(RusticError::new(
            ErrorKind::Configuration,
            "Path is required for source.",
        ))?;
        let ret = LocalBackend::new(config.to_path_buf(), self.clone());
        Ok(Arc::new(ret))
    }
}
