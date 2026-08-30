use crate::rclone::backend::RcloneBackend;
use derive_setters::Setters;
use rustic_core::{BackendConfig, RusticResult, WriteBackend};
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
pub struct RcloneConfig {
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

impl RcloneConfig {
    /// Creates a new [`RcloneRepo`] with the given URL.
    pub fn new(url: impl AsRef<str>) -> Self {
        Self {
            url: Some(url.as_ref().to_string()),
            use_password: None,
            rclone_command: None,
            rest_url: None,
        }
    }
}

impl BackendConfig for RcloneConfig {
    type Output = RcloneBackend;

    fn from_iter<K, V, I>(path: impl AsRef<str>, dict: I) -> Self
    where
        I: IntoIterator<Item=(K, V)>,
        K: Into<String>,
        V: Into<String>
    {
        let mut config = Self::new(path);
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

    fn get_path(&self) -> Option<String> {
        self.url.clone().map(|x| format!("rclone:{}", &x))
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self);
        let _ = ret.remove("url");
        ret
    }

    fn get_repo(&self) -> RusticResult<Self::Output> {
        RcloneBackend::from_config(&self)
    }
}
