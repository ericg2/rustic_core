use crate::rclone::backend::RcloneBackend;
use derive_setters::Setters;
use rustic_core::{RepositoryConfig, RusticResult, WriteBackend};
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use std::sync::Arc;

#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
/// A repository using Rclone.
pub struct RcloneRepo {
    /// The URL to use.
    pub url: Option<String>,

    /// If a password should be used.
    pub use_password: Option<bool>,

    /// The custom rclone command to use.
    pub rclone_command: Option<String>,

    #[serde_as(as = "Option<DisplayFromStr>")]
    /// The REST URL to use (optional).
    pub rest_url: Option<String>,
}

impl RcloneRepo {
    /// Creates a new [`RcloneRepo`] with the given URL.
    pub fn new(url: impl AsRef<str>) -> Self {
        Self {
            url: Some(url.as_ref().to_string()),
            use_password: None,
            rclone_command: None,
            rest_url: None,
        }
    }

    /// Creates a [`RcloneRepo`] from an iterator.
    ///
    /// # Important
    /// This does not guarantee the [`RcloneRepo`] is initialized correctly. Due to the
    /// nature of dynamic types - this feature is only a convenience. All invalid fields will
    /// be skipped, and will not return an error during this process.
    pub fn from_iter<K, V, I>(url: impl AsRef<str>, dict: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut config = Self::new(url);
        for (k, v) in dict {
            let key = k.into();
            let value = v.into();

            match key.as_str() {
                "use-password" => {
                    config.use_password = value.parse().ok();
                }
                "rclone-command" => {
                    config.rclone_command = Some(value);
                }
                "rest-url" => {
                    config.rest_url = Some(value);
                }
                _ => {}
            }
        }

        config
    }
}

impl RepositoryConfig for RcloneRepo {
    fn get_path(&self) -> Option<String> {
        self.url.clone().map(|x| format!("rclone:{}", &x))
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self);
        let _ = ret.remove("url");
        ret
    }

    fn get_repo(&self) -> RusticResult<Arc<dyn WriteBackend>> {
        let ret = RcloneBackend::new(&self)?;
        Ok(Arc::new(ret))
    }
}
