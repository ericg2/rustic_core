use crate::opendal::{OpenDALBackend, Throttle, opendal_add};
use crate::retry::RetrySetting;
use derive_setters::Setters;
use rustic_core::{RepositoryConfig, RusticResult, WriteBackend};
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(windows)]
opendal_add!(
    B2, Ftp, Swift, Azblob, Azdls, Azfile, Cos, Fs, Dropbox, Gdrive, Gcs, Ghac, Http, Ipmfs,
    Memory, Obs, Onedrive, Oss, Pcloud, S3, Webdav, Webhdfs, YandexDisk
);

#[cfg(not(windows))]
opendal_add!(
    B2, Ftp, Swift, Azblob, Azdls, Azfile, Cos, Fs, Dropbox, Gdrive, Gcs, Ghac, Http, Ipmfs,
    Memory, Obs, Onedrive, Oss, Pcloud, S3, Webdav, Webhdfs, YandexDisk, Sftp
);

#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Setters, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
/// Represents a openDAL repository.
pub struct OpenDALConfig {
    /// The maximum connections.
    #[serde(alias = "connections", alias = "max_connections")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub(crate) connections: Option<u32>,

    /// The [`crate::opendal::throttle::Throttle`] settings.
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub(crate) throttle: Option<Throttle>,

    /// The [`RetrySetting`] config.
    #[serde_as(as = "DisplayFromStr")]
    #[serde(default)]
    pub(crate) retry: RetrySetting,

    /// The serialized config.
    #[setters(skip)]
    #[serde(flatten)]
    pub(crate) config: Scheme,
}

impl OpenDALConfig {
    /// Creates an [`OpenDALConfig`] from an iterator.
    ///
    /// # Important
    /// This does not guarantee the [`OpenDALConfig`] is initialized correctly. Due to the
    /// nature of dynamic types - this feature is only a convenience. All invalid fields will
    /// be skipped, and will not return an error during this process.
    pub fn from_iter<K, V, I>(scheme: impl AsRef<str>, dict: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map: HashMap<String, String> = dict
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();

        let connections = map
            .remove("connections")
            .or_else(|| map.remove("max_connections"))
            .and_then(|v| v.parse::<u32>().ok());

        let throttle = map
            .remove("throttle")
            .and_then(|v| v.parse::<Throttle>().ok());

        let retry = map
            .remove("retry")
            .and_then(|v| v.parse::<RetrySetting>().ok())
            .unwrap_or_default();

        Self {
            connections,
            throttle,
            retry,
            config: Scheme::dynamic(scheme, map),
        }
    }

    /// Creates a new openDAL backend via a [`Scheme`].
    ///
    ///
    /// # Arguments
    ///
    /// * `be` - The [`Scheme`] to use.
    pub fn new(be: impl Into<Scheme>) -> Self {
        Self {
            config: be.into(),
            retry: RetrySetting::Default,
            connections: None,
            throttle: None,
        }
    }

    /// # Returns
    ///
    /// The associated [`Scheme`] with this [`OpenDALConfig`].
    pub fn scheme(&self) -> &Scheme {
        &self.config
    }
}

impl RepositoryConfig for OpenDALConfig {
    fn get_path(&self) -> Option<String> {
        self.config.key().map(|x| format!("opendal:{}", &x))
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self);
        let _ = ret.remove("scheme");
        ret.into_iter().collect()
    }

    fn get_repo(&self) -> RusticResult<Arc<dyn WriteBackend>> {
        let ret = OpenDALBackend::new(&self)?;
        Ok(Arc::new(ret))
    }
}
