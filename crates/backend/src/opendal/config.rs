use std::hash::{Hash, Hasher};
use crate::opendal::{OpenDALBackend};
use crate::retry::RetrySetting;
use derive_setters::Setters;
use rustic_core::{RepositoryConfig, RusticResult, WriteBackend};
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use std::sync::Arc;
use opendal_ext::config::OpenDALConfig;

#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
/// An OpenDAL repository.
pub struct OpenDALRepo {
    /// The config to use.
    pub config: OpenDALConfig
}

impl OpenDALRepo {
    /// Creates an [`OpenDALRepo`] from an iterator.
    ///
    /// # Important
    /// This does not guarantee the [`OpenDALRepo`] is initialized correctly. Due to the
    /// nature of dynamic types - this feature is only a convenience. All invalid fields will
    /// be skipped, and will not return an error during this process.
    pub fn from_iter<K, V, I>(scheme: impl AsRef<str>, dict: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>
    {
        Self {
            config: OpenDALConfig::from_iter(scheme, dict)
        }
    }
}

impl RepositoryConfig for OpenDALRepo {
    fn get_path(&self) -> Option<String> {
        Some(format!("opendal:{}", &self.config.config.key()))
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self.config);
        let _ = ret.remove("scheme");
        ret.into_iter().collect()
    }

    fn get_repo(&self) -> RusticResult<Arc<dyn WriteBackend>> {
        let ret = OpenDALBackend::new(&self.config)?;
        Ok(Arc::new(ret))
    }
}
