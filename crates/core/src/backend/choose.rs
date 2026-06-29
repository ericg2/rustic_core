use std::collections::BTreeMap;
use crate::{RepositoryBackends, RepositoryConfig, RusticResult};

/// Options for a backend.
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Default, Debug, serde::Deserialize, serde::Serialize, Eq, PartialEq, Hash)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub struct BackendOptions {
    /// Repository to use
    #[cfg_attr(
        feature = "clap",
        clap(short, long, global = true, visible_alias = "repo", env = "RUSTIC_REPOSITORY", value_hint = ValueHint::DirPath)
    )]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub repository: Option<String>,

    /// Repository to use as hot storage
    #[cfg_attr(
        feature = "clap",
        clap(long, global = true, alias = "repository_hot", env = "RUSTIC_REPO_HOT")
    )]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub repo_hot: Option<String>,

    /// Other options for this repository (hot and cold part)
    #[cfg_attr(feature = "clap", clap(skip))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::btreemap::append_or_ignore))]
    pub options: BTreeMap<String, String>,

    /// Other options for the hot repository
    #[cfg_attr(feature = "clap", clap(skip))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::btreemap::append_or_ignore))]
    pub options_hot: BTreeMap<String, String>,

    /// Other options for the cold repository
    #[cfg_attr(feature = "clap", clap(skip))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::btreemap::append_or_ignore))]
    pub options_cold: BTreeMap<String, String>,
}

impl BackendOptions {
    /// Adds a [`Repository`] using dynamic types.
    pub fn repository(mut self, repo: impl Into<String>) -> Self {
        self.repository = Some(repo.into());
        self
    }

    /// Adds a hot [`Repository`] using dynamic types.
    pub fn repo_hot(mut self, repo: impl Into<String>) -> Self {
        self.repo_hot = Some(repo.into());
        self
    }

    /// Adds a [`Repository`] using a typed config.
    ///
    /// # Important
    /// This will automatically set the configuration. Do not use `options`.
    pub fn with_repo(mut self, repo: &impl RepositoryConfig) -> Self {
        self.repository = repo.get_path();
        self.options_cold = repo.get_options().into_iter().collect();
        self
    }

    /// Adds a hot [`Repository`] using a typed config.
    ///
    /// # Important
    /// This will automatically set the configuration. Do not use `options`.
    pub fn with_repo_hot(mut self, repo: &impl RepositoryConfig) -> Self {
        self.repository = repo.get_path();
        self.options_hot = repo.get_options().into_iter().collect();
        self
    }

    /// Sets the options for all repositories.
    pub fn options<K, V, I>(mut self, dict: I) -> Self
    where
        I: IntoIterator<Item=(K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.options = dict
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    /// Sets the options for the hot repository.
    pub fn options_hot<K, V, I>(mut self, dict: I) -> Self
    where
        I: IntoIterator<Item=(K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.options_hot = dict
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    /// Sets the options for the cold repository.
    pub fn options_cold<K, V, I>(mut self, dict: I) -> Self
    where
        I: IntoIterator<Item=(K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.options_cold = dict
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }
}