/*!
A library for supporting various backends in rustic.

# Overview

This section gives a brief overview of the primary types in this crate:

`rustic_backend` is a support crate for `rustic_core` which provides a way to access a
repository using different backends.

The primary types in this crate are:

- `BackendOptions` - A struct for configuring options for a used backend.
- `SupportedBackend` - An enum for the supported backends.

The following backends are currently supported and can be enabled with features:

- `LocalBackend` - Backend for accessing a local filesystem.
- `OpenDALBackend` - Backend for accessing a `OpenDAL` filesystem.
- `RcloneBackend` - Backend for accessing a Rclone filesystem.
- `RestBackend` - Backend for accessing a REST API.

## Usage & Examples

Due to being a support crate for `rustic_core`, there are no examples here.
Please check the examples in the [`rustic_core`](https://crates.io/crates/rustic_core) crate.

## Crate features

This crate exposes a few features for controlling dependency usage:

- **cli** - Enables support for CLI features by enabling `merge` and `clap`
  features. *This feature is disabled by default*.

- **clap** - Enables a dependency on the `clap` crate and enables parsing from
  the commandline. *This feature is disabled by default*.

- **merge** - Enables support for merging multiple values into one, which
  enables the `conflate` dependency. This is needed for parsing commandline
  arguments and merging them into one (e.g. `config`). *This feature is disabled
  by default*.

### Backend-related features

- **opendal** - Enables support for the `opendal` backend. *This feature is
  enabled by default*.

- **rclone** - Enables support for the `rclone` backend. *This feature is
  enabled by default*.

- **rest** - Enables support for the `rest` backend. *This feature is enabled by
  default*.
*/

// formatting args are used for error messages
#![allow(clippy::literal_string_with_formatting_args)]

pub mod local;

/// `OpenDAL` backend for Rustic.
#[cfg(feature = "opendal")]
pub mod opendal;

/// `Rclone` backend for Rustic.
#[cfg(feature = "rclone")]
pub mod rclone;

/// REST backend for Rustic.
#[cfg(feature = "rest")]
pub mod rest;

pub mod stdin;
pub mod stdout;

mod choose;
mod filter;
mod retry;
mod util;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use serde::Serialize;
use serde_json::Value;
// rustic_backend Public API
pub use crate::choose::{BackendOptions, SupportedBackend};

// re-export for error handling
pub use rustic_core::{ErrorKind, RusticError, RusticResult, Severity, Status};

pub(crate) fn struct_to_map<T: Serialize>(value: &T) -> HashMap<String, String> {
    let v = serde_json::to_value(value).unwrap();
    let obj = v.as_object().expect("expected struct");
    obj.iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string().trim_matches('"').to_string(),
            };
            (k.clone(), val)
        })
        .collect()
}

pub(crate) fn join_force(base: impl AsRef<Path>, p: impl AsRef<Path>) -> PathBuf {
    let mut out = PathBuf::from(base.as_ref());
    for comp in p.as_ref().components() {
        match comp {
            Component::Prefix(_) => {} // skip drive letters / UNC prefix
            Component::RootDir => {}   // skip leading /
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Converts a [`Path`] into an OpenDAL-supported [`String`].
///
/// # Arguments
/// * `base` - The root [`Path`] to use.
/// * `p` - The [`Path`] to convert from.
/// * `is_dir` - If representing a directory or file.
///
/// # Returns
/// A valid [`String`] for OpenDAL use.
pub(crate) fn path_to_str(base: impl AsRef<Path>, p: impl AsRef<Path>, is_dir: bool) -> String {
    let p = crate::join_force(base, p);
    let mut r: String = p.to_string_lossy().to_string();
    if !r.starts_with("/") {
        r = format!("/{r}")
    }
    if is_dir && !r.ends_with("/") {
        r += "/"
    } else if !is_dir && r.ends_with("/") {
        r = r.strip_suffix("/").unwrap_or(&r).to_string()
    }
    r.replace("\\", "/") // *** fix for windows-style directories
}

