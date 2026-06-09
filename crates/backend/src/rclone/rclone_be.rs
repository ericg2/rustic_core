use crate::rest::{RestBackend, RestConfig};
use bytes::Bytes;
use constants::DEFAULT_COMMAND;
use derive_setters::Setters;
use log::{debug, error, info, warn};
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use semver::{BuildMetadata, Prerelease, Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use std::sync::Arc;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    thread::JoinHandle,
};
use url::Url;

use rustic_core::{
    CommandInput, ErrorKind, FileType, Id, ReadBackend, RepositoryConfig, RusticError,
    RusticResult, WriteBackend,
};

pub(super) mod constants {
    /// The default command called if no other is specified
    pub(super) const DEFAULT_COMMAND: &str = "rclone serve restic --addr localhost:0";
    /// The string to search for in the rclone output.
    pub(super) const SEARCH_STRING: &str = "Serving restic REST API on ";
}

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Debug, Setters, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[setters(into)]
#[non_exhaustive]
pub struct RcloneConfig {
    #[setters(skip)]
    #[serde_as(as = "DisplayFromStr")]
    url: Url,

    use_password: Option<bool>,

    rclone_command: Option<String>,

    #[serde_as(as = "Option<DisplayFromStr>")]
    rest_url: Option<Url>,
}

impl RcloneConfig {
    pub fn new(url: impl Into<Url>) -> Self {
        Self {
            url: url.into(),
            use_password: None,
            rclone_command: None,
            rest_url: None,
        }
    }

    //noinspection DuplicatedCode
    pub fn from_iter<K, V, I>(url: impl AsRef<str>, dict: I) -> RusticResult<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map: HashMap<String, String> = dict
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();

        // inject scheme so serde can populate it
        map.insert("url".to_string(), url.as_ref().to_string());

        let config: Self = serde_json::to_value(map)
            .and_then(serde_json::from_value)
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Configuration,
                    "Failed to deserialize OpenDAL config",
                    err,
                )
            })?;

        Ok(config)
    }
}

impl RepositoryConfig for RcloneConfig {
    fn get_path(&self) -> String {
        format!("rclone:{}", self.url.to_string())
    }

    fn get_options(&self) -> HashMap<String, String> {
        let mut ret = crate::struct_to_map(&self);
        ret.remove("url");
        ret
    }

    fn get_repo(&self) -> RusticResult<Arc<dyn WriteBackend>> {
        let ret = RcloneBackend::new(&self)?;
        Ok(Arc::new(ret))
    }
}

/// `RcloneBackend` is a backend that uses rclone to access a remote backend.
#[derive(Debug)]
pub(crate) struct RcloneBackend {
    /// The REST backend.
    rest: RestBackend,
    /// The url of the backend.
    url: String,
    /// The child data contains the child process and is used to kill the child process when the backend is dropped.
    child: Child,
    /// The [`JoinHandle`] of the thread printing rclone's output
    handle: Option<JoinHandle<()>>,
}

impl Drop for RcloneBackend {
    /// Kill the child process.
    fn drop(&mut self) {
        match self.child.kill() {
            Ok(_) => {
                info!("rclone process killed");
                _ = self.handle.take().map(JoinHandle::join);
            }
            Err(_) => warn!("failed to kill rclone. process may be still running!"),
        }
    }
}

/// Check the rclone version.
///
/// # Arguments
///
/// * `rclone_version_output` - The output of `rclone version`.
///
/// # Errors
///
/// * If the rclone version could not be determined or parsed.
/// * If the rclone version is not supported.
///
/// # Returns
///
/// * Ok(()), if the rclone version is supported.
fn check_clone_version(rclone_version_output: &[u8]) -> RusticResult<()> {
    let rclone_version = std::str::from_utf8(rclone_version_output)
        .map_err(|err| {
            RusticError::with_source(
                ErrorKind::Internal,
                "Expected rclone version to be valid utf8, but it was not. Please check the `rclone version` output manually.",
                err,
            )
        })?
        .lines()
        .next()
        .ok_or_else(|| {
            RusticError::new(
                ErrorKind::Internal,
                "Expected rclone version to have at least one line, but it did not. Please check the `rclone version` output manually.",
            )
        })?
        .trim_start_matches(|c: char| !c.is_numeric());

    let mut parsed_version = Version::parse(rclone_version).map_err(|err| {
        RusticError::with_source(ErrorKind::Internal,
                                 "Error parsing rclone version `{version}`. This should not happen. Please check the `rclone version` output manually.",
                                 err)
            .attach_context("version", rclone_version)
    })?;

    // we need to set the pre and build fields to empty to make the comparison work
    // otherwise the comparison will take the pre and build fields into account
    // which would make beta versions pass the check
    parsed_version.pre = Prerelease::EMPTY;
    parsed_version.build = BuildMetadata::EMPTY;

    // for rclone < 1.52.2 setting user/password via env variable doesn't work. This means
    // we are setting up an rclone without authentication which is a security issue!
    // we hard fail here to prevent this, as we can't guarantee the security of the data
    // also because 1.52.2 has been released on Jun 24, 2020, we can assume that this is a
    // reasonable lower bound for the version
    if VersionReq::parse("<1.52.2")
        .map_err(|err| {
            RusticError::with_source(
                ErrorKind::Internal,
                "Error parsing version requirement. This should not happen.",
                err,
            )
        })?
        .matches(&parsed_version)
    {
        return Err(RusticError::new(
            ErrorKind::Unsupported,
            "Unsupported rclone version `{version}`. We must not use rclone without authentication! Please upgrade to rclone >= 1.52.2!",
        )
            .attach_context("version", rclone_version.to_string()));
    }

    Ok(())
}

impl RcloneBackend {
    /// Create a new [`RcloneBackend`] from a given url.
    ///
    /// # Arguments
    ///
    /// * `url` - The url to create the [`RcloneBackend`] from.
    ///
    /// # Errors
    ///
    /// * If the rclone version could not be determined.
    /// * If the rclone version could not be determined.
    /// * If rclone exited with a bad status.
    /// * If the URL does not start with `http`.
    ///
    /// # Returns
    ///
    /// The created [`RcloneBackend`].
    ///
    /// # Panics
    ///
    /// * If the rclone command is not found.
    // TODO: This should be an error, not a panic.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn new(config: &RcloneConfig) -> RusticResult<Self> {
        let rclone_command = config.rclone_command.clone();
        let use_password = config.use_password.unwrap_or(true);

        if use_password && rclone_command.is_none() {
            let rclone_version_output = Command::new("rclone")
                .arg("version")
                .output()
                .map_err(|err| RusticError::with_source(
                    ErrorKind::ExternalCommand,
                    "Experienced an error while running `rclone version` command. Please check if rclone is installed correctly and is in your PATH.",
                    err,
                ))?
                .stdout;

            // if we want to use a password and rclone_command is not explicitly set,
            // we check for a rclone version supporting user/password via env variables
            // if the version is not supported, we return an error
            check_clone_version(rclone_version_output.as_slice())?;
        }

        let user = Alphanumeric.sample_string(&mut rng(), 12);
        let password = Alphanumeric.sample_string(&mut rng(), 12);

        let rclone_command = rclone_command
            .map(|x| x.clone())
            .unwrap_or(DEFAULT_COMMAND.to_string());
        let mut rclone_command: CommandInput = rclone_command.parse().map_err(
            |err| RusticError::with_source(
                ErrorKind::InvalidInput,
                "Expected rclone command to be valid, but it was not. Please check the configuration file.",
                err,
            )
        )?;
        rclone_command.append_arg(config.url.as_ref().to_string());
        debug!("starting rclone via {rclone_command:?}");

        let mut command = Command::new(rclone_command.command());

        if use_password {
            _ = command
                .env("RCLONE_USER", &user)
                .env("RCLONE_PASS", &password);
        }

        let mut child = command
            .args(rclone_command.args())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err|
                RusticError::with_source(
                    ErrorKind::ExternalCommand,
                    "Experienced an error while running rclone: `{rclone_command}`. Please check if rclone is installed and working correctly.",
                    err,
                )
                    .attach_context("rclone_command", rclone_command.to_string())
            )?;

        let mut stderr = BufReader::new(
            child
                .stderr
                .take()
                .ok_or_else(|| RusticError::new(
                    ErrorKind::ExternalCommand,
                    "Could not get stderr of rclone. Please check if rclone is installed and working correctly.",
                ))?,
        );

        let mut rest_url = match config.rest_url.clone() {
            None => {
                loop {
                    if let Some(status) = child.try_wait().map_err(|err|
                        RusticError::with_source(
                            ErrorKind::ExternalCommand,
                            "Experienced an error while running rclone. Please check if rclone is installed and working correctly.",
                            err,
                        )
                    )? {
                        return Err(
                            RusticError::new(
                                ErrorKind::ExternalCommand,
                                "rclone exited before it could start the REST server: `{exit_status}`. Please check the exit status for more information.",
                            ).attach_context("exit_status", status.to_string())
                        );
                    }
                    let mut line = String::new();

                    _ = stderr
                        .read_line(&mut line)
                        .map_err(|err|
                            RusticError::with_source(
                                ErrorKind::InputOutput,
                                "Experienced an error while reading rclone output. Please check if rclone is installed and working correctly.",
                                err,
                            )
                        )?;

                    match line.find(constants::SEARCH_STRING) {
                        Some(result) => {
                            if let Some(url) = line.get(result + constants::SEARCH_STRING.len()..) {
                                // rclone > 1.61 adds brackets around the url, so remove those
                                let brackets: &[_] = &['[', ']'];
                                break url.trim_end().trim_matches(brackets).to_string();
                            }
                        }
                        None if !line.is_empty() => info!("rclone output: {line}"),
                        _ => {}
                    }
                }
            }
            Some(url) => url.to_string(),
        };

        if use_password {
            if !rest_url.starts_with("http://") {
                return Err(RusticError::new(
                    ErrorKind::InputOutput,
                    "Please make sure, the URL `{url}` starts with 'http://'!",
                )
                .attach_context("url", rest_url));
            }
            rest_url = format!("http://{user}:{password}@{}", &rest_url[7..]);
        }

        debug!("using REST backend with url {}.", config.url.as_ref());
        let rest_url = Url::parse(&rest_url).map_err(|err| {
            RusticError::with_source(ErrorKind::InputOutput, "URL is not valid", err)
        })?;

        let rest_config = RestConfig::new(&rest_url);
        let rest_be = RestBackend::new(&rest_config)?;
        let handle = Some(std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                if stderr.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if !line.is_empty() {
                    info!("rclone output: {line}");
                }
            }
        }));

        Ok(Self {
            child,
            url: String::from(config.url.as_ref()),
            rest: rest_be,
            handle,
        })
    }
}

impl ReadBackend for RcloneBackend {
    /// Returns the location of the backend.
    fn location(&self) -> String {
        "rclone:".to_string() + &self.url
    }

    /// Returns the size of the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    ///
    /// If the size could not be determined.
    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        self.rest.list_with_size(tpe)
    }

    /// Reads full data of the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    ///
    /// # Returns
    ///
    /// The data read.
    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        self.rest.read_full(tpe, id)
    }

    /// Reads partial data of the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `cacheable` - Whether the data should be cached.
    /// * `offset` - The offset to read from.
    /// * `length` - The length to read.
    ///
    /// # Returns
    ///
    /// The data read.
    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes> {
        self.rest.read_partial(tpe, id, cacheable, offset, length)
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        // Delegate to the underlying REST backend
        self.rest.warmup_path(tpe, id)
    }
}

impl WriteBackend for RcloneBackend {
    /// Creates a new file.
    fn create(&self) -> RusticResult<()> {
        self.rest.create()
    }

    /// Writes bytes to the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `cacheable` - Whether the data should be cached.
    /// * `buf` - The data to write.
    fn write_bytes(&self, tpe: FileType, id: &Id, cacheable: bool, buf: Bytes) -> RusticResult<()> {
        self.rest.write_bytes(tpe, id, cacheable, buf)
    }

    /// Removes the given file.
    ///
    /// # Arguments
    ///
    /// * `tpe` - The type of the file.
    /// * `id` - The id of the file.
    /// * `cacheable` - Whether the file is cacheable.
    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()> {
        self.rest.remove(tpe, id, cacheable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rstest::rstest;

    #[rstest]
    #[case(b"rclone v1.52.2\n- os/arch: linux/amd64\n- go version: go1.14.4\n")]
    #[case(b"rclone v1.66.0\n- os/version: Microsoft Windows 11 Pro 23H2 (64 bit)\n- os/kernel: 10.0.22631.3155 (x86_64)\n- os/type: windows\n- os/arch: amd64\n- go/version: go1.22.1\n- go/linking: static\n- go/tags: cmount"
    )]
    #[case(b"rclone v1.63.0-beta.7022.e649cf4d5\n- os/arch: linux/amd64\n- go version: go1.14.4\n")]
    fn test_check_clone_version_passes(#[case] rclone_version_output: &[u8]) {
        assert!(check_clone_version(rclone_version_output).is_ok());
    }

    #[rstest]
    #[case(b"")]
    #[case(b"rclone v1.52.1\n- os/arch: linux/amd64\n- go version: go1.14.4\n")]
    #[case(b"rclone v1.51.3-beta\n- os/arch: linux/amd64\n- go version: go1.14.4\n")]
    fn test_check_clone_version_fails(#[case] rclone_version_output: &[u8]) {
        assert!(check_clone_version(rclone_version_output).is_err());
    }
}
