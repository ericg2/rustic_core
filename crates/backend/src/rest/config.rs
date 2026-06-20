use crate::rest::RestBackend;
use crate::retry::RetrySetting;
use derive_setters::Setters;
use jiff::SignedDuration;
use rustic_core::{ErrorKind, RepositoryConfig, RusticError, RusticResult, WriteBackend};
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

fn read_file_contents(log_name: &'static str, path: impl AsRef<Path>) -> RusticResult<String> {
    let mut buf = String::new();
    let _ = std::fs::File::open(path.as_ref())
        .map_err(|err| {
            RusticError::with_source(
                ErrorKind::InvalidInput,
                "Cannot open {log_name} `{path}`",
                err,
            )
            .attach_context("path", path.as_ref().to_string_lossy())
            .attach_context("log_name", log_name)
        })?
        .read_to_string(&mut buf)
        .map_err(|err| {
            RusticError::with_source(
                ErrorKind::InvalidInput,
                "Cannot read {log_name} `{path}`",
                err,
            )
            .attach_context("path", path.as_ref().to_string_lossy())
            .attach_context("log_name", log_name)
        })?;
    Ok(buf)
}

#[serde_as]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Setters, Default)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct RestConfig {
    /// The [`Url`] of the REST backend.
    pub url: Option<String>,

    /// Enable jitter for the backoff.
    ///
    /// # Notes
    ///
    /// When jitter is enabled, [`ExponentialBackoff`] will add a random jitter within `(0, current_delay)`
    /// to the current delay.
    pub jitter: Option<bool>,

    /// Set the factor for the backoff.
    ///
    /// # Notes
    ///
    /// Having a factor less than `1.0` does not make any sense as it would create a
    /// smaller negative backoff.
    pub factor: Option<f32>,

    /// Set the minimum delay for the backoff.
    pub min_delay: Option<SignedDuration>,

    /// Set the maximum delay for the backoff.
    pub max_delay: Option<SignedDuration>,

    /// Set the maximum number of attempts for the current backoff.
    #[serde_as(as = "DisplayFromStr")]
    pub retry: RetrySetting,

    /// Set the total timeout.
    pub timeout: Option<SignedDuration>,

    /// Sets the User-Agent of the REST backend.
    pub user_agent: Option<String>,

    /// Sets the CA Root [`Certificate`] of the REST backend.
    #[serde(alias = "ca_cert", alias = "cacert")]
    #[serde(rename = "cacert")]
    #[setters(skip)]
    pub ca_cert: Option<String>,

    /// Sets the TLS Client [`Identity`] of the REST backend.
    #[setters(skip)]
    pub tls_client_cert: Option<String>,
}

impl RestConfig {
    /// Creates a new [`RestConfig`] with the given URI.
    pub fn new(url: impl AsRef<str>) -> Self {
        Self {
            url: Some(url.as_ref().to_string()),
            jitter: None,
            factor: None,
            min_delay: None,
            max_delay: None,
            retry: RetrySetting::Default,
            timeout: None,
            user_agent: None,
            ca_cert: None,
            tls_client_cert: None,
        }
    }

    /// Creates a [`RestConfig`] from an iterator.
    ///
    /// # Important
    /// This does not guarantee the [`RestConfig`] is initialized correctly. Due to the
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
                "jitter" => {
                    config.jitter = value.parse().ok();
                }

                "factor" => {
                    config.factor = value.parse().ok();
                }

                "min-delay" => {
                    config.min_delay = value.parse().ok();
                }

                "max-delay" => {
                    config.max_delay = value.parse().ok();
                }

                "retry" => {
                    if let Ok(retry) = value.parse() {
                        config.retry = retry;
                    }
                }

                "timeout" => {
                    config.timeout = value.parse().ok();
                }

                "user-agent" => {
                    config.user_agent = Some(value);
                }

                "cacert" | "ca-cert" | "ca_cert" => {
                    config.ca_cert = Some(value);
                }

                "tls-client-cert" => {
                    config.tls_client_cert = Some(value);
                }

                _ => {}
            }
        }

        config
    }

    /// Sets the CA Cert [`Certificate`] from an encoded string.
    pub fn ca_cert(mut self, cert: impl Into<Option<String>>) -> Self {
        self.ca_cert = cert.into();
        self
    }

    /// Retrieves the CA Root [`Certificate`] from a local file.
    ///
    /// # Arguments
    /// * `path` - The [`Path`] to read from.
    ///
    /// # Errors
    /// * If the [`Path`] does not exist.
    /// * If the file is not a valid CA [`Certificate`].
    ///
    /// # Notes
    /// The file does not need to exist after success. The contents will be dumped.
    pub fn ca_cert_file(mut self, path: impl AsRef<Path>) -> RusticResult<Self> {
        self.ca_cert = Some(read_file_contents("cacert", path)?);
        Ok(self)
    }

    /// Retrieves the TLS Client [`Identity`] from a local file.
    ///
    /// # Arguments
    /// * `path` - The [`Path`] to read from.
    ///
    /// # Errors
    /// * If the [`Path`] does not exist.
    /// * If the file is not a valid TLS Client [`Identity`]
    ///
    /// # Notes
    /// The file does not need to exist after success. The contents will be dumped.
    pub fn tls_client_file(mut self, path: impl AsRef<Path>) -> RusticResult<Self> {
        self.tls_client_cert = Some(read_file_contents("tls-client-cert", path)?);
        Ok(self)
    }

    /// Sets the TLS Client [`Identity`] from an encoded string.
    pub fn tls_client_cert(mut self, cert: impl Into<Option<String>>) -> Self {
        self.tls_client_cert = cert.into();
        self
    }
}

impl RepositoryConfig for RestConfig {
    fn get_path(&self) -> Option<String> {
        self.url.clone().map(|x| format!("rest:{}", &x))
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self);
        let _ = ret.remove("url");
        ret
    }

    fn get_repo(&self) -> RusticResult<Arc<dyn WriteBackend>> {
        let ret = RestBackend::new(&self)?;
        Ok(Arc::new(ret))
    }
}
