//! This module contains [`BackendOptions`] and helpers to choose a backend from a given url.
use std::fmt::Debug;
use std::{collections::BTreeMap, sync::Arc};
use strum::{Display, EnumString};

use rustic_core::{BackendOptions, ErrorKind, RepositoryBackends, RepositoryConfig, RusticError, RusticResult, WriteBackend};

use crate::util::{BackendLocation, location_to_type_and_path};

use crate::local::LocalRepo;
use crate::opendal::OpenDALRepo;
use crate::rclone::RcloneRepo;
use crate::rest::RestRepo;
#[cfg(feature = "clap")]
use clap::ValueHint;

pub trait BackendBuilder: Send + Sync + 'static {
    fn to_backends(&self) -> RusticResult<RepositoryBackends>;
}

impl BackendBuilder for BackendOptions {
    fn to_backends(&self) -> RusticResult<RepositoryBackends> {
        let mut options = self.options.clone();
        options.extend(self.options_cold.clone());
        let be = get_backend(self.repository.as_ref(), options)?
            .ok_or_else(|| {
                RusticError::new(
                    ErrorKind::Backend,
                    "No repository given. Please make sure, that you have set the repository.",
                )
            })?;
        let mut options = self.options.clone();
        options.extend(self.options_hot.clone());
        let be_hot = get_backend(self.repo_hot.as_ref(), options)?;
        Ok(RepositoryBackends::new(be, be_hot))
    }
}

fn get_backend(
    repo_string: Option<&String>,
    options: BTreeMap<String, String>,
) -> RusticResult<Option<Arc<dyn WriteBackend>>> {
    repo_string
        .map(|string| {
            let (be_type, location) = location_to_type_and_path(string)?;
            be_type
                .to_backend(location.clone(), options.into())
                .map_err(|err| {
                    err
                        .prepend_guidance_line("Could not load the backend `{name}` at `{location}`. Please check the given backend and try again.")
                        .attach_context("name", be_type.to_string())
                        .attach_context("location", location.to_string())
                })
        })
        .transpose()
}

/// Trait which can be implemented to choose a backend from a backend type, a backend path and options given as `HashMap`.
pub trait BackendChoice {
    /// Init backend from a path and options.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to create that points to the backend.
    /// * `options` - additional options for creating the backend
    ///
    /// # Errors
    ///
    /// * If the backend is not supported.
    fn to_backend(
        &self,
        location: BackendLocation,
        options: Option<BTreeMap<String, String>>,
    ) -> RusticResult<Arc<dyn WriteBackend>>;
}

/// The supported backend types.
///
/// Currently supported types are "local", "rclone", "rest", "opendal"
///
/// # Notes
///
/// If the url is a windows path, the type will be "local".
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, Display)]
pub enum SupportedBackend {
    /// A local backend
    #[strum(serialize = "local", to_string = "Local Backend")]
    Local,

    #[cfg(feature = "rclone")]
    /// A rclone backend
    #[strum(serialize = "rclone", to_string = "rclone Backend")]
    Rclone,

    #[cfg(feature = "rest")]
    /// A REST backend
    #[strum(serialize = "rest", to_string = "REST Backend")]
    Rest,

    #[cfg(feature = "opendal")]
    /// An openDAL backend (general)
    #[strum(serialize = "opendal", to_string = "openDAL Backend")]
    OpenDAL,
}

impl SupportedBackend {
    fn map_config(
        &self,
        location: BackendLocation,
        options: Option<BTreeMap<String, String>>,
    ) -> RusticResult<Arc<dyn RepositoryConfig>> {
        let options = options.unwrap_or_default();
        Ok(match self {
            Self::Local => Arc::new(LocalRepo::from_iter(location, options)),
            #[cfg(feature = "rclone")]
            Self::Rclone => Arc::new(RcloneRepo::from_iter(location, options)),
            #[cfg(feature = "rest")]
            Self::Rest => Arc::new(RestRepo::from_iter(location, options)),
            #[cfg(feature = "opendal")]
            Self::OpenDAL => Arc::new(OpenDALRepo::from_iter(location, options)),
        })
    }
}

impl BackendChoice for SupportedBackend {
    fn to_backend(
        &self,
        location: BackendLocation,
        options: Option<BTreeMap<String, String>>,
    ) -> RusticResult<Arc<dyn WriteBackend>> {
        let map = self.map_config(location, options)?;
        map.get_repo()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("local", SupportedBackend::Local)]
    #[cfg(feature = "rclone")]
    #[case("rclone", SupportedBackend::Rclone)]
    #[cfg(feature = "rest")]
    #[case("rest", SupportedBackend::Rest)]
    #[cfg(feature = "opendal")]
    #[case("opendal", SupportedBackend::OpenDAL)]
    fn test_try_from_is_ok(#[case] input: &str, #[case] expected: SupportedBackend) {
        assert_eq!(SupportedBackend::try_from(input).unwrap(), expected);
    }

    #[test]
    fn test_try_from_unknown_is_err() {
        assert!(SupportedBackend::try_from("unknown").is_err());
    }
}
