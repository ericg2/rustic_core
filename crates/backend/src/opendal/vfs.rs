// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use crate::BackendBuilder;
use log::warn;
use opendal::raw::oio::Entry;
use opendal::raw::*;
use opendal::{
    Buffer, Builder, Capability, Configurator, EntryMode, Error, ErrorKind, Metadata,
    OperationContext,
};
use rustic_core::vfs::{IdenticalSnapshot, Latest, OpenFile, Vfs};
use rustic_core::{
    BackendOptions, Credentials, IndexedFullStatus, Node, Repository, RepositoryOptions,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};
use std::vec;
use tokio::sync::{OnceCell, RwLock};
use tokio::time;

/// Configuration for the Rustic VFS OpenDAL backend.
///
/// `RusticVfsConfig` holds all settings required to construct a
/// [`RusticVfsBuilder`] and ultimately a [`VfsBackend`] backed by a
/// [rustic](https://github.com/rustic-rs/rustic) repository. It implements
/// [`Configurator`], so it can be handed directly to OpenDAL's service-builder
/// machinery via [`into_builder`](Configurator::into_builder).
///
/// This config is **rustic-specific** — it is not a general-purpose VFS
/// abstraction. All fields map directly to rustic concepts and are forwarded
/// as-is to the underlying rustic storage layer.
///
/// # Field requirements
///
/// All fields are wrapped in `Option` so that [`Default`] can be derived and
/// configs can be partially constructed (e.g. loaded from a file and then
/// patched). However, some fields are **logically required** and will cause
/// [`RusticVfsBuilder::build`] to return a
/// [`ConfigInvalid`](opendal::ErrorKind::ConfigInvalid) error if left
/// as `None`:
///
/// | Field              | Required at build time | Notes |
/// |--------------------|------------------------|-------|
/// | `options`          | ✓                      | Must describe a valid repository |
/// | `backend`          | ✓                      | Must point to reachable storage |
/// | `credentials`      | ✓                      | Repository will not open without these |
/// | `refresh_interval` | –                      | Defaults to 5 minutes when `None` |
///
#[derive(Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
//#[doc = include_str!("docs.md")]
pub struct RusticVfsConfig {
    /// Options that describe the rustic repository to open.
    ///
    /// Controls how the repository is located (path, URL) and how its password
    /// is sourced (env var, file, command). Forwarded verbatim to
    /// [`Repository::new`](rustic_core::Repository::new).
    pub options: RepositoryOptions,

    /// Low-level backend options forwarded to the rustic storage layer.
    ///
    /// Governs how rustic physically accesses its storage (local disk, S3,
    /// SFTP, rclone, etc.), including repository and hot-cache paths.
    pub backend: BackendOptions,

    /// Credentials used to authenticate against the rustic repository.
    ///
    /// Passed to [`Repository::open`](rustic_core::Repository::open). There
    /// is no ambient/fallback credential lookup — these must be supplied
    /// explicitly. If authentication fails a
    /// [`ConfigInvalid`](opendal::ErrorKind::ConfigInvalid) error is
    /// returned at build time.
    ///
    /// **Logically required** — [`build`](Builder::build) returns
    /// [`ConfigInvalid`](opendal::ErrorKind::ConfigInvalid) if `None`.
    /// Wrapped in `Option` solely to satisfy [`Default`].
    pub credentials: Option<Credentials>,

    /// How often the VFS layer should re-read the rustic repository index.
    ///
    /// When set, a background task wakes at this cadence, rebuilds the
    /// in-memory VFS from the latest snapshots, and swaps it in atomically so
    /// that snapshots written by other writers become visible without restarting
    /// the process.
    ///
    /// Serialized / deserialized as a human-readable duration string
    /// (e.g. `"30s"`, `"5m"`) via [`humantime_serde`]. Defaults to **5
    /// minutes** when `None`.
    #[serde(default, with = "humantime_serde")]
    pub refresh_interval: Option<Duration>,
}

impl Configurator for RusticVfsConfig {
    type Builder = RusticVfsBuilder;

    /// Consumes the config and returns a [`RusticVfsBuilder`] ready for
    /// further customization or an immediate call to [`build`](Builder::build).
    fn into_builder(self) -> Self::Builder {
        RusticVfsBuilder { config: self }
    }
}

/// Builder for the Rustic VFS OpenDAL backend.
///
/// `RusticVfsBuilder` wraps a [`RusticVfsConfig`] and exposes a fluent setter
/// API so that individual fields can be overridden after a config has been
/// loaded (e.g. from a file) but before the backend is constructed.
///
/// This builder is **rustic-specific** — it targets rustic repositories
/// exclusively and is not a general-purpose VFS builder. All setters map
/// directly to rustic concepts.
///
/// Obtain a builder either via [`RusticVfsConfig::into_builder`] or from the
/// [`Default`] impl when starting from a blank slate.
///
/// # Required setters
///
/// [`with_options`](RusticVfsBuilder::with_options),
/// [`with_backend`](RusticVfsBuilder::with_backend), and
/// [`with_credentials`](RusticVfsBuilder::with_credentials) **must** be called
/// before [`build`](Builder::build); omitting any of them will return a
/// [`ConfigInvalid`](opendal::ErrorKind::ConfigInvalid) error.
///
#[derive(Debug, Default, Clone)]
pub struct RusticVfsBuilder {
    pub(super) config: RusticVfsConfig,
}

impl RusticVfsBuilder {
    /// Sets the [`RepositoryOptions`] that describe the rustic repository.
    ///
    /// Controls how the repository is located and opened (path, URL, password
    /// source, etc.). Forwarded verbatim to the rustic layer.
    ///
    /// **Required before [`build`](Builder::build).**
    ///
    /// # Arguments
    ///
    /// * `options` – Repository options describing the target rustic repository.
    pub fn with_options(mut self, options: RepositoryOptions) -> Self {
        self.config.options = options;
        self
    }

    /// Sets the [`BackendOptions`] used by the rustic storage layer.
    ///
    /// Governs how rustic physically accesses its storage (local disk, S3,
    /// SFTP, rclone, etc.), including repository and hot-cache paths.
    ///
    /// **Required before [`build`](Builder::build).**
    ///
    /// # Arguments
    ///
    /// * `backend` – Low-level storage backend options for the rustic backend.
    pub fn with_backend(mut self, backend: BackendOptions) -> Self {
        self.config.backend = backend;
        self
    }

    /// Sets the [`Credentials`] used to authenticate against the rustic
    /// repository.
    ///
    /// There is no ambient/fallback credential lookup — credentials must be
    /// supplied explicitly here or via [`RusticVfsConfig`].
    ///
    /// **Required before [`build`](Builder::build).**
    ///
    /// # Arguments
    ///
    /// * `credentials` – Credentials for rustic repository authentication.
    pub fn with_credentials(mut self, credentials: Credentials) -> Self {
        self.config.credentials = Some(credentials);
        self
    }

    /// Sets the periodic refresh interval for the rustic index cache.
    ///
    /// When set, a background task rebuilds the in-memory VFS at this cadence
    /// so that snapshots written by other writers become visible without
    /// restarting the process. Pass `None` to fall back to the default of
    /// **5 minutes**.
    ///
    /// # Arguments
    ///
    /// * `interval` – How often to re-read the rustic repository index.
    pub fn with_refresh_interval(mut self, interval: impl Into<Option<Duration>>) -> Self {
        self.config.refresh_interval = interval.into();
        self
    }
}

impl Builder for RusticVfsBuilder {
    type Config = RusticVfsConfig;

    /// Consumes the builder and constructs a [`VfsBackend`] from the current
    /// rustic configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigInvalid`](opendal::ErrorKind::ConfigInvalid) if
    /// any of the logically required fields (`options`, `backend`,
    /// `credentials`) were not set, or if the rustic repository cannot be
    /// opened with the supplied configuration. Returns
    /// [`Unexpected`](opendal::ErrorKind::Unexpected) for transient
    /// failures (e.g. the storage backend is temporarily unreachable).
    fn build(self) -> opendal::Result<impl Service> {
        VfsBackend::from_config(self.config)
    }
}

// ── type aliases ─────────────────────────────────────────────────────────────

/// A fully-opened, index-loaded rustic repository.
///
/// The [`IndexedFullStatus`] type parameter signals that rustic has read the
/// repository index into memory, making pack lookups and node resolution
/// available without further I/O.
type IndexedRepo = Repository<IndexedFullStatus>;

// ── constants ─────────────────────────────────────────────────────────────────

/// Fallback refresh cadence used when [`RusticVfsConfig::refresh_interval`] is
/// `None`. Set to 5 minutes as a balance between freshness and repository I/O.
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Default snapshot path template passed to [`Vfs::from_snapshots`].
///
/// Tokens like `{hostname}` and `{label}` are expanded by rustic at VFS
/// build time; `{time}` is formatted with [`DEFAULT_TIME`].
const DEFAULT_PATH: &str = "[{hostname}]/[{label}]/[{time}]";

/// [`strftime`](https://docs.rs/chrono/latest/chrono/format/strftime)-style
/// format string used to render the `{time}` token in [`DEFAULT_PATH`].
const DEFAULT_TIME: &str = "%Y-%m-%d-%H-%M-%S";

/// A standard "this backend is read-only" error, reused by every mutating
/// operation (`create_dir`, `write`, `delete`, `copy`, `rename`, `presign`).
fn unsupported(op: &'static str) -> Error {
    Error::new(
        ErrorKind::Unsupported,
        format!("VfsBackend is read-only; `{op}` is not supported."),
    )
}

// ── VfsBackend ────────────────────────────────────────────────────────────────

/// OpenDAL [`Service`] implementation backed by a rustic repository.
///
/// `VfsBackend` presents the snapshots in a rustic repository as a read-only
/// virtual filesystem. The in-memory VFS is kept up-to-date by a background
/// refresh task that periodically re-reads the repository index and
/// atomically swaps in a new [`Vfs`] instance.
///
/// # Capabilities
///
/// | Capability        | Supported |
/// |-------------------|-----------|
/// | `stat`            | ✓         |
/// | `read`            | ✓         |
/// | `list`            | ✓         |
/// | `list_recursive`  | –         |
/// | `copy`            | –         |
/// | writes / deletes  | –         |
///
/// # Construction
///
/// Prefer [`VfsBackend::from_config`] when building from a
/// [`RusticVfsConfig`], or [`VfsBackend::from_repo`] when you already hold an
/// open [`IndexedRepo`].
#[derive(Debug)]
pub struct VfsBackend {
    /// Shared, refresh-able VFS.
    ///
    /// Writers hold the lock only long enough to swap in a freshly-built
    /// instance; readers (`stat` / `read` / `list`) hold a short-lived read
    /// guard and release it before returning.
    vfs: Arc<RwLock<Vfs>>,

    /// The open rustic repository used for blob and node lookups.
    ///
    /// Wrapped in `Arc` so it can be shared with the background refresh task
    /// without cloning the repository itself.
    repo: Arc<IndexedRepo>,
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a fresh [`Vfs`] from all snapshots currently in `repo`.
///
/// Snapshots are arranged into a directory tree using [`DEFAULT_PATH`] and
/// [`DEFAULT_TIME`]. Duplicate snapshots with identical content are presented
/// as directories ([`IdenticalSnapshot::AsDir`]) and the `latest` symlink
/// entry is also a directory ([`Latest::AsDir`]).
///
/// # Errors
///
/// Propagates any rustic error (e.g. index read failure, corrupt pack) as an
/// [`opendal::ErrorKind::Unexpected`] temporary error so that the caller
/// can retry without discarding the existing VFS.
fn build_vfs(repo: &IndexedRepo) -> opendal::Result<Vfs> {
    repo.get_all_snapshots()
        .and_then(|snapshots| {
            Vfs::from_snapshots(
                snapshots,
                DEFAULT_PATH,
                DEFAULT_TIME,
                Latest::AsDir,
                IdenticalSnapshot::AsDir,
            )
        })
        .map_err(|e| {
            Error::new(ErrorKind::Unexpected, "Failed to build VFS.")
                .set_source(e)
                .set_temporary()
        })
}

// ── background refresh task ───────────────────────────────────────────────────

/// Spawns a Tokio task that atomically rebuilds the shared [`Vfs`] on
/// `interval`.
///
/// The task holds only a [`Weak`] reference to the shared `RwLock<Vfs>`; when
/// the owning [`VfsBackend`] is dropped, the `Weak` upgrade fails and the task
/// exits cleanly on the next tick — no explicit cancellation is needed.
///
/// On a successful rebuild the write lock is held only for the duration of the
/// pointer swap, keeping read-side latency impact minimal. On failure the
/// existing VFS is left intact and a warning is logged so that reads continue
/// serving stale-but-valid data.
///
/// # Arguments
///
/// * `vfs`      – Shared reference to the VFS being managed.
/// * `repo`     – Open rustic repository used to rebuild the VFS.
/// * `interval` – How often to attempt a rebuild.
fn spawn_refresh_task(vfs: &Arc<RwLock<Vfs>>, repo: Arc<IndexedRepo>, interval: Duration) {
    let weak_vfs: Weak<RwLock<Vfs>> = Arc::downgrade(vfs);
    let mut ticker = time::interval(interval);
    let _ = tokio::spawn(async move {
        loop {
            ticker.tick().await;
            let Some(arc_vfs) = weak_vfs.upgrade() else {
                break;
            };
            let repo_clone = repo.clone();
            // Move blocking work off the executor thread
            match tokio::task::spawn_blocking(move || build_vfs(&repo_clone)).await {
                Ok(Ok(new_vfs)) => {
                    *arc_vfs.write().await = new_vfs;
                }
                Ok(Err(e)) => warn!("VFS refresh failed: {e}"),
                Err(e) => warn!("VFS refresh panicked: {e}"),
            }
        }
    });
}

// ── VfsBackend impl ───────────────────────────────────────────────────────────
impl VfsBackend {
    /// Construct a [`VfsBackend`] from an already-opened, indexed rustic
    /// repository.
    ///
    /// Builds the initial in-memory VFS synchronously, then spawns a
    /// background task to refresh it every `refresh_interval`. Use
    /// [`DEFAULT_REFRESH_INTERVAL`] if you have no specific cadence in mind.
    ///
    /// Prefer [`from_config`](VfsBackend::from_config) when you are starting
    /// from a [`RusticVfsConfig`] rather than a pre-opened repository.
    ///
    /// # Arguments
    ///
    /// * `repo`             – An open, indexed rustic repository.
    /// * `refresh_interval` – How often the background task rebuilds the VFS.
    ///
    /// # Errors
    ///
    /// Returns [`Unexpected`](opendal::ErrorKind::Unexpected) if the
    /// initial VFS build fails (e.g. the repository index is unreadable).
    pub fn from_repo(repo: Arc<IndexedRepo>, refresh_interval: Duration) -> opendal::Result<Self> {
        let vfs = Arc::new(RwLock::new(build_vfs(&repo)?));

        let backend = Self { vfs, repo };

        spawn_refresh_task(&backend.vfs, backend.repo.clone(), refresh_interval);

        Ok(backend)
    }

    /// Construct a [`VfsBackend`] from a [`RusticVfsConfig`].
    ///
    /// Opens the rustic repository described by the config (authenticating with
    /// the supplied credentials), loads its index, and then delegates to
    /// [`from_repo`](VfsBackend::from_repo).
    ///
    /// This is the primary entry point used by [`RusticVfsBuilder::build`].
    ///
    /// # Errors
    ///
    /// | Condition | Error kind |
    /// |-----------|-----------|
    /// | `credentials` is `None` | [`ConfigInvalid`](opendal::ErrorKind::ConfigInvalid) |
    /// | `backend` options cannot be parsed | [`ConfigInvalid`](opendal::ErrorKind::ConfigInvalid) |
    /// | Repository cannot be opened / indexed | [`Unexpected`](opendal::ErrorKind::Unexpected) (temporary) |
    pub fn from_config(config: RusticVfsConfig) -> opendal::Result<Self> {
        let creds = config.credentials.ok_or_else(|| {
            Error::new(
                ErrorKind::ConfigInvalid,
                "Credentials must be supplied via `RusticVfsConfig::credentials`.",
            )
        })?;

        let be = config.backend.to_backends().map_err(|e| {
            Error::new(ErrorKind::ConfigInvalid, "Failed to parse backend config.")
                .set_source(e)
                .with_context("repo", config.backend.repository.unwrap_or_default())
                .with_context("repo_hot", config.backend.repo_hot.unwrap_or_default())
                .set_permanent()
        })?;

        let repo = Repository::new(&config.options, &be)
            .and_then(|r| r.open(&creds))
            .and_then(|r| r.to_indexed())
            .map_err(|e| {
                Error::new(
                    ErrorKind::Unexpected,
                    "Failed to open rustic repository. Check that options and credentials are correct.",
                )
                    .set_source(e)
                    .set_temporary()
            })?;

        Self::from_repo(
            Arc::new(repo),
            config.refresh_interval.unwrap_or(DEFAULT_REFRESH_INTERVAL),
        )
    }

    /// Resolve a VFS path string to a rustic [`Node`].
    ///
    /// Normalizes the path (ensuring a leading `/` and stripping trailing
    /// slashes) before handing it to the VFS. Holds the VFS read lock only for
    /// the duration of the lookup.
    ///
    /// # Errors
    ///
    /// Returns [`NotFound`](opendal::ErrorKind::NotFound) if the path
    /// does not exist in the current VFS snapshot.
    async fn node_from_path(&self, path: &str) -> opendal::Result<Node> {
        let path = normalize_path(path);
        self.vfs
            .read()
            .await
            .node_from_path(&self.repo, Path::new(&path))
            .map_err(|e| Error::new(ErrorKind::NotFound, "Path not found in VFS.").set_source(e))
    }
}

// ── Service impl ───────────────────────────────────────────────────────────────

impl Service for VfsBackend {
    type Reader = oio::PositionReader<VfsReader>;
    type Writer = ();
    type Lister = VfsLister;
    type Deleter = ();
    type Copier = ();

    fn info(&self) -> ServiceInfo {
        ServiceInfo::new("rustic", "/", "rustic")
    }

    fn capability(&self) -> Capability {
        Capability {
            stat: true,
            read: true,
            list: true,
            shared: true,
            list_with_recursive: false,
            ..Default::default()
        }
    }

    fn create_dir(
        &self,
        _ctx: &OperationContext,
        _path: &str,
        _args: OpCreateDir,
    ) -> impl Future<Output = opendal::Result<RpCreateDir>> + MaybeSend {
        async { Err(unsupported("create_dir")) }
    }

    /// Stat a path, returning its [`Metadata`] (type and last-modified time).
    ///
    /// This is eager — unlike `read`/`list`, `stat` needs the resolved
    /// [`Metadata`] immediately to build its [`RpStat`], so it resolves the
    /// node right away rather than deferring the lookup.
    ///
    /// # Errors
    ///
    /// Returns [`NotFound`](opendal::ErrorKind::NotFound) if the path
    /// does not exist in the current VFS snapshot.
    async fn stat(
        &self,
        _ctx: &OperationContext,
        path: &str,
        _args: OpStat,
    ) -> opendal::Result<RpStat> {
        let node = self.node_from_path(path).await?;
        Ok(RpStat::new(meta_from_node(&node)))
    }

    /// Construct a [`VfsReader`] for `path`.
    ///
    /// This is intentionally synchronous and does **no** I/O: it doesn't
    /// resolve the path to a [`Node`] and doesn't open the underlying rustic
    /// file. That work happens lazily, the first time [`VfsReader::open`] or
    /// [`VfsReader::read`] is called (and is cached after that), which is
    /// also why no [`RpRead`] is returned here — it's produced later,
    /// alongside the resolved [`Metadata`], by the reader itself.
    fn read(
        &self,
        _ctx: &OperationContext,
        path: &str,
        _args: OpRead,
    ) -> opendal::Result<Self::Reader> {
        let normalized = normalize_path(path);
        let reader = VfsReader::new(normalized, self.vfs.clone(), self.repo.clone());
        Ok(oio::PositionReader::new(reader))
    }

    fn write(
        &self,
        _ctx: &OperationContext,
        _path: &str,
        _args: OpWrite,
    ) -> opendal::Result<Self::Writer> {
        Err(unsupported("write"))
    }

    fn delete(&self, _ctx: &OperationContext) -> opendal::Result<Self::Deleter> {
        Err(unsupported("delete"))
    }

    /// Construct a [`VfsLister`] for `path`.
    ///
    /// Like `read`, this is synchronous and doesn't touch the VFS — the
    /// directory's children are fetched lazily on the first call to
    /// [`VfsLister::next`] and cached from then on.
    fn list(
        &self,
        _ctx: &OperationContext,
        path: &str,
        _args: OpList,
    ) -> opendal::Result<Self::Lister> {
        let normalized = normalize_path(path);
        let path_buf = Path::new(&normalized).to_path_buf();
        Ok(VfsLister::new(
            path_buf,
            self.vfs.clone(),
            self.repo.clone(),
        ))
    }

    fn copy(
        &self,
        _ctx: &OperationContext,
        _from: &str,
        _to: &str,
        _args: OpCopy,
        _opts: OpCopier,
    ) -> opendal::Result<Self::Copier> {
        Err(unsupported("copy"))
    }

    fn rename(
        &self,
        _ctx: &OperationContext,
        _from: &str,
        _to: &str,
        _args: OpRename,
    ) -> impl Future<Output = opendal::Result<RpRename>> + MaybeSend {
        async { Err(unsupported("rename")) }
    }

    fn presign(
        &self,
        _ctx: &OperationContext,
        _path: &str,
        _args: OpPresign,
    ) -> impl Future<Output = opendal::Result<RpPresign>> + MaybeSend {
        async { Err(unsupported("presign")) }
    }
}

// ── metadata / path helpers ───────────────────────────────────────────────────

/// Convert a rustic [`Node`] to OpenDAL [`Metadata`].
///
/// Maps the node's directory/file status, last-modified time, and content
/// length into the fields OpenDAL consumers expect. `Metadata::with_last_modified`
/// takes an [`opendal::raw::Timestamp`] (jiff-backed) rather than a
/// `chrono::DateTime`, so the node's `mtime` is routed through `SystemTime`
/// first via `TryFrom<SystemTime>`, which works regardless of whether rustic's
/// `mtime` is a `chrono::DateTime<_>` or `time::OffsetDateTime` under the hood.
fn meta_from_node(n: &Node) -> Metadata {
    let mode = if n.is_dir() {
        EntryMode::DIR
    } else {
        EntryMode::FILE
    };

    let mut meta = Metadata::new(mode).with_content_length(n.meta.size);

    let timestamp = n
        .meta
        .mtime
        .and_then(|mtime| Timestamp::try_from(SystemTime::from(mtime)).ok())
        .unwrap_or_else(|| {
            Timestamp::new(0, 0).expect("Unix epoch is a valid OpenDAL timestamp")
        });

    meta.with_last_modified(timestamp)
}


/// Normalize a path string for VFS lookup.
///
/// Ensures the path starts with `/` and strips any trailing `/` unless the
/// path is the root (`/`) itself. OpenDAL may hand us paths in either form;
/// rustic's VFS expects the leading slash.
fn normalize_path(path: &str) -> String {
    let p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if p.len() > 1 {
        p.trim_end_matches('/').to_string()
    } else {
        p
    }
}

// ── VfsReader ─────────────────────────────────────────────────────────────────

/// [`oio::PositionRead`] implementation that pulls blob data out of a rustic
/// repository file.
///
/// `VfsReader` is fully lazy: it's constructed with just a `path` and shared
/// handles to the [`Vfs`] and [`IndexedRepo`] — no node resolution, no file
/// open. The first call to [`open`](oio::PositionRead::open) resolves the
/// path to a [`Node`] and opens the underlying rustic file via
/// [`ensure_open`](VfsReader::ensure_open), caching the result in a
/// [`OnceCell`] so later calls (or later `open`s of the same reader) reuse
/// the same handle instead of repeating the lookup/open.
pub struct VfsReader {
    /// The VFS path this reader was constructed for.
    path: String,
    /// Shared, refresh-able VFS, used to lazily resolve `path` to a [`Node`].
    vfs: Arc<RwLock<Vfs>>,
    /// Repository used to open the file and fetch blob data for each chunk.
    repo: Arc<IndexedRepo>,
    /// Lazily-populated `(resolved node, opened file)`, computed once on
    /// first use and reused thereafter.
    state: OnceCell<(Node, Arc<OpenFile>)>,
}

/// Per-`open()` handle for a [`VfsReader`].
///
/// Carries everything [`VfsReader::read_at`] needs to service a request
/// without touching the VFS again: the already-open rustic `file`, the
/// `repo` used to read chunks out of it, and the node's `content_length`
/// (captured once, from the same [`Node`] [`VfsReader::ensure_open`]
/// resolved). That cached `content_length` is what lets `read_at` clamp
/// every request to the file's actual size instead of trusting the caller's
/// `offset`/`size` — the same pattern [`MountReader`] uses.
pub struct VfsHandle {
    repo: Arc<IndexedRepo>,
    file: Arc<OpenFile>,
    content_length: usize,
}

impl oio::PositionRead for VfsReader {
    type Handle = VfsHandle;

    /// Resolve the node and open the rustic file (via
    /// [`ensure_open`](VfsReader::ensure_open)), then package everything
    /// [`read_at`](Self::read_at) needs — including the node's
    /// `content_length`, captured up front so every later read can be
    /// clamped to it — into a [`VfsHandle`].
    async fn open(&self) -> opendal::Result<Self::Handle> {
        let (node, file) = self.ensure_open().await?;

        Ok(VfsHandle {
            repo: self.repo.clone(),
            file,
            content_length: node.meta.size as usize,
        })
    }

    /// Read up to `size` bytes starting at `offset`.
    ///
    /// Both are clamped against `handle.content_length` before anything is
    /// sent to rustic: `offset` is capped at the file's end, and `size` is
    /// trimmed to however many bytes actually remain from there. A request
    /// that starts at or past EOF resolves to an empty [`Buffer`] without
    /// issuing a read at all, rather than asking `Repository::read_file_at`
    /// for bytes that were never going to be there.
    async fn read_at(handle: &Self::Handle, offset: u64, size: usize) -> opendal::Result<Buffer> {
        let offset = (offset as usize).min(handle.content_length);
        let remaining = handle.content_length.saturating_sub(offset);
        let read_size = size.min(remaining);

        if read_size == 0 {
            return Ok(Buffer::new());
        }

        let repo = handle.repo.clone();
        let file = handle.file.clone();

        let data = tokio::task::spawn_blocking(move || repo.read_file_at(&file, offset, read_size))
            .await
            .map_err(|e| Error::new(ErrorKind::Unexpected, "join error").set_source(e))?
            .map_err(|e| Error::new(ErrorKind::Unexpected, "read failed").set_source(e))?;

        Ok(data.into())
    }
}

impl VfsReader {
    /// Create a new, unopened [`VfsReader`] for `path`.
    ///
    /// Does no I/O. The path isn't resolved and the file isn't opened until
    /// [`ensure_open`](VfsReader::ensure_open) runs, which happens the first
    /// time [`open`](oio::PositionRead::open) is called.
    ///
    /// # Arguments
    ///
    /// * `path` – Normalised VFS path to read.
    /// * `vfs`  – Shared VFS used to resolve `path` to a [`Node`].
    /// * `repo` – Repository used to open the file and resolve blob data.
    pub(crate) fn new(
        path: impl Into<String>,
        vfs: Arc<RwLock<Vfs>>,
        repo: Arc<IndexedRepo>,
    ) -> Self {
        Self {
            path: path.into(),
            vfs,
            repo,
            state: OnceCell::new(),
        }
    }

    /// Resolve `path` to a [`Node`] and open the underlying rustic file,
    /// doing so only once and caching the result for subsequent calls.
    ///
    /// # Errors
    ///
    /// Returns [`NotFound`](opendal::ErrorKind::NotFound) if the path
    /// doesn't exist in the current VFS snapshot, or
    /// [`Unexpected`](opendal::ErrorKind::Unexpected) (temporary) if the
    /// rustic file can't be opened.
    async fn ensure_open(&self) -> opendal::Result<(Node, Arc<OpenFile>)> {
        let (node, file) = self
            .state
            .get_or_try_init(|| async {
                let node = self
                    .vfs
                    .read()
                    .await
                    .node_from_path(&self.repo, Path::new(&self.path))
                    .map_err(|e| {
                        Error::new(ErrorKind::NotFound, "Path not found in VFS.").set_source(e)
                    })?;

                let repo = self.repo.clone();
                let node_for_open = node.clone();
                let file = tokio::task::spawn_blocking(move || repo.open_file(&node_for_open))
                    .await
                    .map_err(|e| Error::new(ErrorKind::Unexpected, "join error").set_source(e))?
                    .map_err(|e| {
                        Error::new(
                            ErrorKind::Unexpected,
                            "Failed to open file in rustic backend.",
                        )
                        .set_source(e)
                        .set_temporary()
                    })?;

                Ok::<_, Error>((node, Arc::new(file)))
            })
            .await?;

        Ok((node.clone(), file.clone()))
    }
}

// ── VfsLister ─────────────────────────────────────────────────────────────────

/// [`oio::List`] implementation that iterates over the direct children of a
/// rustic VFS directory.
///
/// Like [`VfsReader`], `VfsLister` is lazy: it's constructed with just the
/// target `root` path and shared `vfs`/`repo` handles, doing no I/O. The
/// directory's children are fetched only on the first call to
/// [`next`](oio::List::next), then cached in `nodes` for subsequent calls.
pub struct VfsLister {
    /// The directory path being listed (used to build child entry paths).
    root: PathBuf,
    /// Shared, refresh-able VFS, used to lazily fetch `root`'s children.
    vfs: Arc<RwLock<Vfs>>,
    /// Repository passed through to the VFS lookup.
    repo: Arc<IndexedRepo>,
    /// Lazily-populated child-node iterator; `None` until the first
    /// [`next`](oio::List::next) call.
    nodes: Option<vec::IntoIter<Node>>,
}

impl VfsLister {
    /// Create a new, unloaded [`VfsLister`] for `root`.
    ///
    /// Does no I/O; `root`'s children aren't fetched until the first call to
    /// [`next`](oio::List::next).
    ///
    /// # Arguments
    ///
    /// * `root` – Normalised path of the directory being listed.
    /// * `vfs`  – Shared VFS used to fetch `root`'s children.
    /// * `repo` – Repository passed through to the VFS lookup.
    pub(crate) fn new(root: PathBuf, vfs: Arc<RwLock<Vfs>>, repo: Arc<IndexedRepo>) -> Self {
        Self {
            root,
            vfs,
            repo,
            nodes: None,
        }
    }

    /// Fetch `root`'s children on first call, caching them in `self.nodes`
    /// for subsequent calls.
    ///
    /// # Errors
    ///
    /// Returns [`NotFound`](opendal::ErrorKind::NotFound) if `root` doesn't
    /// exist or isn't a directory in the current VFS snapshot.
    async fn ensure_loaded(&mut self) -> opendal::Result<&mut vec::IntoIter<Node>> {
        if self.nodes.is_none() {
            let entries = self
                .vfs
                .read()
                .await
                .dir_entries_from_path(&self.repo, &self.root)
                .map_err(|e| {
                    Error::new(ErrorKind::NotFound, "Directory not found in VFS.").set_source(e)
                })?;
            self.nodes = Some(entries.into_iter());
        }

        Ok(self.nodes.as_mut().expect("just populated above"))
    }
}

impl oio::List for VfsLister {
    /// Return the next directory entry, or `None` when the listing is
    /// exhausted.
    ///
    /// Entry paths are constructed as `{root}/{node.name}`, with a trailing
    /// `/` appended for directories (as required by OpenDAL's path convention).
    async fn next(&mut self) -> opendal::Result<Option<Entry>> {
        let base = self.root.to_string_lossy().replace('\\', "/");
        let base = base.trim_matches('/').to_string();

        let iter = self.ensure_loaded().await?;
        let entry = iter.next().map(|n| {
            let mut path = if base.is_empty() {
                n.name.clone()
            } else {
                format!("{base}/{}", n.name)
            };

            if n.is_dir() {
                if !path.ends_with('/') {
                    path.push('/');
                }
            } else {
                path = path.trim_end_matches('/').to_string();
            }

            Entry::new(&path, meta_from_node(&n))
        });

        Ok(entry)
    }
}
