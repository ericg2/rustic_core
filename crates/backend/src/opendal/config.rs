//! Configuration types for the `rustic` OpenDAL backend.
//!
//! This module defines [`OpenDALConfig`], the [`RepositoryConfig`] implementation used to
//! configure an [`OpenDALRepo`] backend, along with its two "typed" sub-options:
//! [`Throttle`] (bandwidth/burst rate limiting) and [`Retry`] (retry policy).
//!
//! [`OpenDALConfig`] stores a handful of well-known, strongly typed settings (`throttle`,
//! `connections`, `retry`) plus an open-ended bag of scheme-specific `options` (as required by
//! the underlying `opendal` crate, whose per-scheme option keys are not known statically).
//!
//! [`OpenDALConfig::get_options`] (via [`RepositoryConfig`]) flattens the typed fields back
//! into string key/value pairs (merged with `options`) for consumption by `opendal`.
//! [`OpenDALConfig::from_iter`] is the inverse of that operation: given such a flattened
//! key/value iterator, it reconstructs the typed fields and leaves everything else in
//! `options`.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use bytesize::ByteSize;
use derive_setters::Setters;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use rustic_core::{BackendConfig, ErrorKind, RusticError, RusticResult, WriteBackend, WriteSource};

use crate::opendal::OpenDALSource;

/// Throttling parameters for an OpenDAL backend, expressed as a token-bucket
/// rate limiter: a sustained `bandwidth` (bytes/sec) and a `burst` capacity
/// (bytes), both stored as raw byte counts.
///
/// # Parsing
///
/// [`Throttle`] implements [`FromStr`] so it can be read from a string of the
/// form `"<bandwidth>,<burst>"`, where each side is anything [`ByteSize`]
/// understands (e.g. `"10kiB,10MB"`).
///
/// # Display
///
/// The [`fmt::Display`] impl is the exact inverse of [`FromStr`]: formatting a
/// [`Throttle`] and re-parsing the result yields an equal value. This
/// round-trip property is what [`OpenDALConfig::get_options`] and
/// [`OpenDALConfig::from_iter`] rely on to serialize/deserialize the
/// `"throttle"` option as a single string.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default, Eq, PartialEq)]
pub struct Throttle {
    /// Sustained bandwidth limit, in bytes per second.
    pub bandwidth: u32,
    /// Burst capacity, in bytes.
    pub burst: u32,
}

impl FromStr for Throttle {
    type Err = Box<RusticError>;

    /// Parses a `"<bandwidth>,<burst>"` string (e.g. `"10kiB,10MB"`) into a
    /// [`Throttle`].
    ///
    /// # Errors
    ///
    /// Returns an error if either side is missing, is not a valid
    /// [`ByteSize`], or overflows `u32` once converted to raw bytes.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut values = s
            .split(',')
            .map(|s| {
                ByteSize::from_str(s.trim()).map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::InvalidInput,
                        "Parsing ByteSize from throttle string `{string}` failed",
                        err,
                    )
                    .attach_context("string", s)
                })
            })
            .map(|b| -> RusticResult<u32> {
                let bytesize = b?.as_u64();
                bytesize.try_into().map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::Internal,
                        "Converting ByteSize `{bytesize}` to u32 failed",
                        err,
                    )
                    .attach_context("bytesize", bytesize.to_string())
                })
            });

        let bandwidth = values
            .next()
            .transpose()?
            .ok_or_else(|| RusticError::new(ErrorKind::MissingInput, "No bandwidth given."))?;

        let burst = values
            .next()
            .transpose()?
            .ok_or_else(|| RusticError::new(ErrorKind::MissingInput, "No burst given."))?;

        Ok(Self { bandwidth, burst })
    }
}

impl fmt::Display for Throttle {
    /// Formats this [`Throttle`] back into the `"<bandwidth>,<burst>"` form
    /// accepted by [`FromStr`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{},{}",
            ByteSize::b(self.bandwidth as u64),
            ByteSize::b(self.burst as u64),
        )
    }
}

/// Retry policy for failed OpenDAL operations.
///
/// Serializes to / parses from the string option `"retry"` as follows:
/// - [`Retry::Off`] <-> `"off"`
/// - [`Retry::Default`] <-> `"default"`
/// - [`Retry::Custom(n)`] <-> the decimal string `"n"` (number of retries)
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default, Eq, PartialEq)]
pub enum Retry {
    /// Disable retries entirely.
    Off,
    /// Use the backend's default retry behavior.
    #[default]
    Default,
    /// Retry up to a fixed, caller-specified number of times.
    Custom(usize),
}

/// Configuration for an [`OpenDALRepo`] backend.
///
/// This is the [`RepositoryConfig`] implementation for OpenDAL-backed
/// repositories. It combines a small set of strongly-typed, well-known
/// settings (`throttle`, `connections`, `retry`) with an open-ended
/// `options` map for scheme-specific settings that `opendal` itself defines
/// (and which this crate does not attempt to enumerate).
///
/// # Round-tripping
///
/// [`OpenDALConfig::get_options`] (from [`RepositoryConfig`]) and
/// [`OpenDALConfig::from_iter`] are inverses of one another: encoding a
/// config with `get_options` and decoding the result with `from_iter` (using
/// the same `scheme`) reproduces an equivalent [`OpenDALConfig`], and vice
/// versa, modulo values that fail to parse (see `from_iter`'s docs).
#[serde_as]
#[derive(Clone, Debug, Setters, Serialize, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct OpenDALConfig {
    /// The OpenDAL scheme to use (e.g. `"s3"`, `"fs"`, `"gcs"`). Encoded into
    /// the config "path" as `opendal:<scheme>`.
    pub scheme: Option<String>,
    /// Scheme-specific options passed through verbatim to `opendal`, plus
    /// any unrecognized keys encountered by [`OpenDALConfig::from_iter`].
    /// Does not include `"throttle"`, `"connections"`, or `"retry"`, which
    /// are represented by the dedicated typed fields below.
    pub options: HashMap<String, String>,
    /// Optional bandwidth/burst rate limiting. Encoded as the `"throttle"`
    /// option.
    pub throttle: Option<Throttle>,
    /// Optional cap on the number of concurrent connections. Encoded as the
    /// `"connections"` option.
    pub connections: Option<usize>,
    /// Optional retry policy. Encoded as the `"retry"` option; always
    /// present in [`OpenDALConfig::get_options`]'s output (defaulting to
    /// `"default"` when unset).
    pub retry: Option<Retry>,
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
        let mut options = HashMap::new();
        let mut throttle = None;
        let mut connections = None;
        let mut retry = None;

        for (k, v) in dict {
            let k = k.into();
            let v = v.into();
            match k.as_str() {
                "throttle" => throttle = Throttle::from_str(&v).ok(),
                "connections" => connections = v.parse().ok(),
                "retry" => {
                    retry = Some(match v.as_str() {
                        "off" => Retry::Off,
                        "default" => Retry::Default,
                        n => n.parse().map(Retry::Custom).unwrap_or_default(),
                    });
                }
                _ => {
                    let _ = options.insert(k, v);
                }
            }
        }

        Self {
            scheme: Some(scheme.as_ref().to_string()),
            options,
            throttle,
            connections,
            retry,
        }
    }

    pub fn build(self) -> RusticResult<OpenDALSource> {
        OpenDALSource::from_config(&self)
    }
}

impl BackendConfig for OpenDALConfig {
    /// Returns the config "path" as `opendal:<scheme>`, or [`None`] if no
    /// scheme is set.
    fn get_path(&self) -> Option<String> {
        self.scheme.as_ref().map(|x| format!("opendal:{}", x))
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = HashMap::new();
        if let Some(throttle) = self.throttle {
            ret.insert("throttle".into(), throttle.to_string());
        }

        if let Some(connections) = self.connections {
            ret.insert("connections".into(), connections.to_string());
        }

        ret.insert(
            "retry".into(),
            match self.retry {
                Some(Retry::Custom(x)) => x.to_string(),
                Some(Retry::Off) => "off".into(),
                _ => "default".into(),
            },
        );

        ret.extend(self.options.clone());
        ret
    }

    fn get_source(&self) -> RusticResult<Arc<dyn WriteSource>> {
        let be = OpenDALSource::from_config(config)?;
        Ok(Arc::new(be))
    }
}
