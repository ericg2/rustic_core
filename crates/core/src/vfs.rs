mod format;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt::Write as _,
    path::{Component, Path, PathBuf},
};

use bytes::{Bytes, BytesMut};
use runtime_format::FormatArgs;
use strum::EnumString;

use crate::{
    blob::{BlobId, DataId, tree::TreeId},
    error::{ErrorKind, RusticError, RusticResult},
    index::ReadIndex,
    repofile::{BlobType, Metadata, Node, NodeType, SnapshotFile},
    repository::{IndexedFull, Repository},
    vfs::format::FormattedSnapshot,
};

/// Name of the virtual summary file placed in the root of the [`Vfs`]
const SUMMARY_FILE_NAME: &str = "SUMMARY.txt";

/// [`VfsErrorKind`] describes the errors that can be returned from the Virtual File System
#[derive(thiserror::Error, Debug, displaydoc::Display)]
pub enum VfsErrorKind {
    /// Directory exists as non-virtual directory
    DirectoryExistsAsNonVirtual,
    /// Only normal paths allowed
    OnlyNormalPathsAreAllowed,
    /// Name `{0:?}` doesn't exist
    NameDoesNotExist(OsString),
}

pub(crate) type VfsResult<T> = Result<T, VfsErrorKind>;

#[derive(Debug, Clone, Copy)]
/// `IdenticalSnapshot` describes how to handle identical snapshots.
pub enum IdenticalSnapshot {
    /// create a link to the previous identical snapshots
    AsLink,
    /// make a dir, i.e. don't add special treatment for identical snapshots
    AsDir,
}

#[derive(Debug, Clone, Copy)]
/// `Latest` describes whether a `latest` entry should be added.
pub enum Latest {
    /// Add `latest` as directory with identical content as the last snapshot by time.
    AsDir,
    /// Add `latest` as symlink
    AsLink,
    /// Don't add a `latest` entry
    No,
}

#[derive(Debug)]
/// A potentially virtual tree in the [`Vfs`]
enum VfsTree {
    /// A symlink to the given link target
    Link(OsString),
    /// A repository tree; id of the tree
    RusticTree(TreeId),
    /// A purely virtual tree containing subtrees
    VirtualTree(BTreeMap<OsString, Self>),
    /// A purely virtual file with in-memory content
    File(Bytes),
}

#[derive(Debug)]
/// A resolved path within a [`Vfs`]
enum VfsPath<'a> {
    /// Path is the given symlink
    Link(&'a OsString),
    /// Path is within repository, give the tree [`Id`] and remaining path.
    RusticPath(&'a TreeId, PathBuf),
    /// Path is the given virtual tree
    VirtualTree(&'a BTreeMap<OsString, VfsTree>),
    /// Path is a purely virtual file with the given content
    File(&'a Bytes),
}

impl VfsTree {
    /// Create a new [`VfsTree`]
    fn new() -> Self {
        Self::VirtualTree(BTreeMap::new())
    }

    /// Add some tree to this root tree at the given path
    ///
    /// # Arguments
    ///
    /// * `path` - The path to add the tree to
    /// * `new_tree` - The tree to add
    ///
    /// # Errors
    ///
    /// * If the path is not a normal path
    /// * If the path is a directory in the repository
    ///
    /// # Returns
    ///
    /// `Ok(())` if the tree was added successfully
    fn add_tree(&mut self, path: &Path, new_tree: Self) -> VfsResult<()> {
        let mut tree = self;
        let mut components = path.components();
        let Some(Component::Normal(last)) = components.next_back() else {
            return Err(VfsErrorKind::OnlyNormalPathsAreAllowed);
        };

        for comp in components {
            if let Component::Normal(name) = comp {
                match tree {
                    Self::VirtualTree(virtual_tree) => {
                        tree = virtual_tree
                            .entry(name.to_os_string())
                            .or_insert(Self::VirtualTree(BTreeMap::new()));
                    }
                    _ => {
                        return Err(VfsErrorKind::DirectoryExistsAsNonVirtual);
                    }
                }
            }
        }

        let Self::VirtualTree(virtual_tree) = tree else {
            return Err(VfsErrorKind::DirectoryExistsAsNonVirtual);
        };

        _ = virtual_tree.insert(last.to_os_string(), new_tree);
        Ok(())
    }

    /// Get the tree at this given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to get the tree for
    ///
    /// # Errors
    ///
    // TODO: Document errors
    ///
    /// # Returns
    ///
    /// If the path is within a real repository tree, this returns the [`VfsTree::RusticTree`] and the remaining path
    fn get_path(&self, path: &Path) -> VfsResult<VfsPath<'_>> {
        let mut tree = self;
        let mut components = path.components();
        loop {
            match tree {
                Self::RusticTree(id) => {
                    let path: PathBuf = components.collect();
                    return Ok(VfsPath::RusticPath(id, path));
                }
                Self::VirtualTree(virtual_tree) => match components.next() {
                    Some(Component::Normal(name)) => {
                        if let Some(new_tree) = virtual_tree.get(name) {
                            tree = new_tree;
                        } else {
                            return Err(VfsErrorKind::NameDoesNotExist(name.to_os_string()));
                        }
                    }
                    None => {
                        return Ok(VfsPath::VirtualTree(virtual_tree));
                    }

                    _ => {}
                },
                Self::Link(target) => return Ok(VfsPath::Link(target)),
                Self::File(content) => return Ok(VfsPath::File(content)),
            }
        }
    }
}

#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, EnumString, serde::Deserialize, serde::Serialize)]
#[strum(ascii_case_insensitive)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
/// Policy to describe how to handle access to a file
pub enum FilePolicy {
    /// Don't allow reading the file
    Forbidden,
    /// Read the file
    Read,
}

#[derive(Debug)]
/// A virtual file system which offers repository contents
pub struct Vfs {
    /// The root tree
    tree: VfsTree,
}

// ---------------------------------------------------------------------------
// Summary rendering helpers
// ---------------------------------------------------------------------------

/// Format an integer with thousands separators: `1234567` -> `1,234,567`
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Format a byte count using binary units: `1536` -> `1.50 KiB`
#[allow(clippy::cast_precision_loss)]
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// Format a duration in seconds: `3725.0` -> `1h 02m 05s`
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn human_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "-".to_string();
    }
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else {
        format!("{m}m {s:02}s")
    }
}

/// Write one aligned `label  value` row
fn row(out: &mut String, label: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "  {:<13}{value}", format!("{label}:"));
}

/// Join a list of strings, or `-` if empty
fn join_or_dash<'a>(items: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = items.map(String::as_str).collect();
    if v.is_empty() {
        "-".to_string()
    } else {
        v.join(", ")
    }
}

/// Render a human readable summary of all given snapshots.
fn render_summary(snapshots: &[SnapshotFile]) -> String {
    const WIDTH: usize = 72;
    let heavy = "═".repeat(WIDTH);
    let mut out = String::new();

    // ---- Header / overview ------------------------------------------------
    let _ = writeln!(out, "{heavy}");
    let _ = writeln!(out, "  REPOSITORY SNAPSHOT SUMMARY");
    let _ = writeln!(out, "{heavy}");
    out.push('\n');

    let time_fmt = "%Y-%m-%d %H:%M:%S %Z";
    let hosts: BTreeSet<&str> = snapshots.iter().map(|s| s.hostname.as_str()).collect();
    let with_summary = snapshots.iter().filter(|s| s.summary.is_some()).count();
    let total_added_packed: u64 = snapshots
        .iter()
        .filter_map(|s| s.summary.as_ref())
        .map(|s| s.data_added_packed)
        .sum();
    let total_added: u64 = snapshots
        .iter()
        .filter_map(|s| s.summary.as_ref())
        .map(|s| s.data_added)
        .sum();
    let with_errors = snapshots
        .iter()
        .filter_map(|s| s.summary.as_ref())
        .filter(|s| s.error_count > 0)
        .count();

    row(&mut out, "Snapshots", thousands(snapshots.len() as u64));
    row(
        &mut out,
        "Hosts",
        if hosts.is_empty() {
            "-".to_string()
        } else {
            hosts.into_iter().collect::<Vec<_>>().join(", ")
        },
    );
    if let (Some(first), Some(last)) = (snapshots.first(), snapshots.last()) {
        row(&mut out, "Oldest", first.time.strftime(time_fmt));
        row(&mut out, "Newest", last.time.strftime(time_fmt));
    }
    row(
        &mut out,
        "With stats",
        format!("{with_summary} of {}", snapshots.len()),
    );
    row(
        &mut out,
        "Total added",
        format!(
            "{} (packed: {})",
            human_bytes(total_added),
            human_bytes(total_added_packed)
        ),
    );
    if with_errors > 0 {
        row(
            &mut out,
            "Had errors",
            format!("{with_errors} snapshot(s) had unreadable sources"),
        );
    }
    out.push('\n');

    // ---- Per snapshot -----------------------------------------------------
    let total = snapshots.len();
    for (i, snap) in snapshots.iter().enumerate() {
        let id = snap.id.to_string();
        let short_id: String = id.chars().take(8).collect();

        let title = format!(" [{}/{}] {} ", i + 1, total, short_id);
        let pad = WIDTH.saturating_sub(title.chars().count() + 2);
        let _ = writeln!(out, "──{title}{}", "─".repeat(pad));

        row(&mut out, "ID", &id);
        row(&mut out, "Time", snap.time.strftime(time_fmt));
        row(
            &mut out,
            "Host/User",
            format!(
                "{} / {} (uid {}, gid {})",
                if snap.hostname.is_empty() { "-" } else { &snap.hostname },
                if snap.username.is_empty() { "-" } else { &snap.username },
                snap.uid,
                snap.gid
            ),
        );
        row(&mut out, "Paths", join_or_dash(snap.paths.iter()));
        row(&mut out, "Tags", join_or_dash(snap.tags.iter()));
        if !snap.label.is_empty() {
            row(&mut out, "Label", &snap.label);
        }
        if let Some(desc) = &snap.description {
            row(&mut out, "Description", desc);
        }
        if let Some(parent) = &snap.parent {
            row(&mut out, "Parent", parent.to_string());
        }
        if let Some(original) = &snap.original {
            row(&mut out, "Original", original.to_string());
        }
        row(&mut out, "Tree", snap.tree.to_string());
        if !snap.program_version.is_empty() {
            row(&mut out, "Program", &snap.program_version);
        }

        if let Some(s) = &snap.summary {
            out.push('\n');
            if !s.command.is_empty() {
                row(&mut out, "Command", &s.command);
            }
            row(
                &mut out,
                "Backup",
                format!(
                    "{}  →  {}",
                    s.backup_start.strftime(time_fmt),
                    s.backup_end.strftime(time_fmt)
                ),
            );
            row(
                &mut out,
                "Duration",
                format!(
                    "{} backup, {} total",
                    human_duration(s.backup_duration),
                    human_duration(s.total_duration)
                ),
            );
            row(
                &mut out,
                "Files",
                format!(
                    "{} processed, {}  ({} new, {} changed, {} unmodified)",
                    thousands(s.total_files_processed),
                    human_bytes(s.total_bytes_processed),
                    thousands(s.files_new),
                    thousands(s.files_changed),
                    thousands(s.files_unmodified),
                ),
            );
            row(
                &mut out,
                "Dirs",
                format!(
                    "{} processed, {}  ({} new, {} changed, {} unmodified)",
                    thousands(s.total_dirs_processed),
                    human_bytes(s.total_dirsize_processed),
                    thousands(s.dirs_new),
                    thousands(s.dirs_changed),
                    thousands(s.dirs_unmodified),
                ),
            );
            row(
                &mut out,
                "Added",
                format!(
                    "{} (packed: {})",
                    human_bytes(s.data_added),
                    human_bytes(s.data_added_packed)
                ),
            );
            row(
                &mut out,
                "  of files",
                format!(
                    "{} (packed: {})",
                    human_bytes(s.data_added_files),
                    human_bytes(s.data_added_files_packed)
                ),
            );
            row(
                &mut out,
                "  of trees",
                format!(
                    "{} (packed: {})",
                    human_bytes(s.data_added_trees),
                    human_bytes(s.data_added_trees_packed)
                ),
            );
            row(
                &mut out,
                "New blobs",
                format!(
                    "{} data, {} tree",
                    thousands(s.data_blobs),
                    thousands(s.tree_blobs)
                ),
            );
            if s.error_count > 0 {
                row(
                    &mut out,
                    "Errors",
                    format!("{} source(s) could not be read", thousands(s.error_count)),
                );
            }
        } else {
            row(&mut out, "Stats", "not available for this snapshot");
        }
        out.push('\n');
    }

    let _ = writeln!(out, "{heavy}");
    out
}

impl Vfs {
    /// Create a new [`Vfs`] from a directory [`Node`].
    ///
    /// # Arguments
    ///
    /// * `node` - The directory [`Node`] to create the [`Vfs`] from
    ///
    /// # Panics
    ///
    /// * If the node is not a directory
    #[must_use]
    pub fn from_dir_node(node: &Node) -> Self {
        let tree = VfsTree::RusticTree(node.subtree.unwrap());
        Self { tree }
    }

    /// Create a new [`Vfs`] from a list of snapshots.
    ///
    /// A virtual `SUMMARY.txt` file describing all snapshots is added to the root.
    ///
    /// # Arguments
    ///
    /// * `snapshots` - The snapshots to create the [`Vfs`] from
    /// * `path_template` - The template for the path of the snapshots
    /// * `time_template` - The template for the time of the snapshots
    /// * `latest_option` - Whether to add a `latest` entry
    /// * `id_snap_option` - Whether to add a link to identical snapshots
    ///
    /// # Errors
    ///
    /// * If the path is not a normal path
    /// * If the path is a directory in the repository
    #[allow(clippy::too_many_lines)]
    pub fn from_snapshots(
        mut snapshots: Vec<SnapshotFile>,
        path_template: &str,
        time_template: &str,
        latest_option: Latest,
        id_snap_option: IdenticalSnapshot,
    ) -> RusticResult<Self> {
        snapshots.sort_unstable();
        let mut tree = VfsTree::new();

        // render the summary while we still own all snapshots (sorted by time)
        let summary_text = render_summary(&snapshots);

        // to handle identical trees
        let mut last_parent = None;
        let mut last_name = None;
        let mut last_tree = TreeId::default();

        // to handle "latest" entries
        let mut dirs_for_link = BTreeMap::new();
        let mut dirs_for_snap = BTreeMap::new();

        for snap in snapshots {
            let path = FormatArgs::new(
                path_template,
                &FormattedSnapshot {
                    snap: &snap,
                    time_format: time_template,
                },
            )
                .to_string();
            let path = Path::new(&path);
            let filename = path.file_name().map(OsStr::to_os_string);
            let parent_path = path.parent().map(Path::to_path_buf);

            // Save paths for latest entries, if requested
            if matches!(latest_option, Latest::AsLink) {
                _ = dirs_for_link.insert(parent_path.clone(), filename.clone());
            }
            if matches!(latest_option, Latest::AsDir) {
                _ = dirs_for_snap.insert(parent_path.clone(), snap.tree);
            }

            // Create the entry, potentially as symlink if requested
            if last_parent != parent_path || last_name != filename {
                if matches!(id_snap_option, IdenticalSnapshot::AsLink)
                    && last_parent == parent_path
                    && last_tree == snap.tree
                {
                    if let Some(name) = last_name {
                        tree.add_tree(path, VfsTree::Link(name.clone()))
                            .map_err(|err| {
                                RusticError::with_source(
                                    ErrorKind::Vfs,
                                    "Failed to add a link `{name}` to root tree at `{path}`",
                                    err,
                                )
                                    .attach_context("path", path.display().to_string())
                                    .attach_context("name", name.to_string_lossy())
                                    .ask_report()
                            })?;
                    }
                } else {
                    tree.add_tree(path, VfsTree::RusticTree(snap.tree))
                        .map_err(|err| {
                            RusticError::with_source(
                                ErrorKind::Vfs,
                                "Failed to add repository tree `{tree_id}` to root tree at `{path}`",
                                err,
                            )
                                .attach_context("path", path.display().to_string())
                                .attach_context("tree_id", snap.tree.to_string())
                                .ask_report()
                        })?;
                }
            }
            last_parent = parent_path;
            last_name = filename;
            last_tree = snap.tree;
        }

        // Add latest entries if requested
        match latest_option {
            Latest::No => {}
            Latest::AsLink => {
                for (path, target) in dirs_for_link {
                    if let (Some(mut path), Some(target)) = (path, target) {
                        path.push("latest");
                        tree.add_tree(&path, VfsTree::Link(target.clone()))
                            .map_err(|err| {
                                RusticError::with_source(
                                    ErrorKind::Vfs,
                                    "Failed to link latest `{target}` entry to root tree at `{path}`",
                                    err,
                                )
                                    .attach_context("path", path.display().to_string())
                                    .attach_context("target", target.to_string_lossy())
                                    .attach_context("latest", "link")
                                    .ask_report()
                            })?;
                    }
                }
            }
            Latest::AsDir => {
                for (path, subtree) in dirs_for_snap {
                    if let Some(mut path) = path {
                        path.push("latest");
                        tree.add_tree(&path, VfsTree::RusticTree(subtree))
                            .map_err(|err| {
                                RusticError::with_source(
                                    ErrorKind::Vfs,
                                    "Failed to add latest subtree id `{id}` to root tree at `{path}`",
                                    err,
                                )
                                    .attach_context("path", path.display().to_string())
                                    .attach_context("tree_id", subtree.to_string())
                                    .attach_context("latest", "dir")
                                    .ask_report()
                            })?;
                    }
                }
            }
        }

        // Add the virtual SUMMARY.txt to the root
        tree.add_tree(
            Path::new(SUMMARY_FILE_NAME),
            VfsTree::File(Bytes::from(summary_text)),
        )
            .map_err(|err| {
                RusticError::with_source(
                    ErrorKind::Vfs,
                    "Failed to add virtual file `{name}` to root tree",
                    err,
                )
                    .attach_context("name", SUMMARY_FILE_NAME)
                    .ask_report()
            })?;

        Ok(Self { tree })
    }

    /// Get a [`Node`] from the specified path.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository to get the [`Node`] from
    /// * `path` - The path to get the [`Tree`] at
    ///
    /// # Errors
    ///
    /// * If the component name doesn't exist
    ///
    /// # Returns
    ///
    /// The [`Node`] at the specified path
    ///
    /// [`Tree`]: crate::repofile::Tree
    pub fn node_from_path<S: IndexedFull>(
        &self,
        repo: &Repository<S>,
        path: &Path,
    ) -> RusticResult<Node> {
        let meta = Metadata::default();
        match self.tree.get_path(path).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Vfs,
                "Failed to get tree at given path `{path}`",
                err,
            )
                .attach_context("path", path.display().to_string())
                .ask_report()
        })? {
            VfsPath::RusticPath(tree_id, path) => Ok(repo.node_from_path(*tree_id, &path)?),
            VfsPath::VirtualTree(_) => {
                Ok(Node::new(String::new(), NodeType::Dir, meta, None, None))
            }
            VfsPath::Link(target) => Ok(Node::new(
                String::new(),
                NodeType::from_link(Path::new(target)),
                meta,
                None,
                None,
            )),
            VfsPath::File(content) => {
                let meta = Metadata {
                    size: content.len() as u64,
                    ..Metadata::default()
                };
                Ok(Node::new(String::new(), NodeType::File, meta, None, None))
            }
        }
    }

    /// Get a list of [`Node`]s from the specified directory path.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository to get the [`Node`] from
    /// * `path` - The path to get the [`Tree`] at
    ///
    /// # Errors
    ///
    /// * If the component name doesn't exist
    ///
    /// # Returns
    ///
    /// The list of [`Node`]s at the specified path
    ///
    /// [`Tree`]: crate::repofile::Tree
    ///
    /// # Panics
    ///
    /// * Panics if the path is not a directory.
    pub fn dir_entries_from_path<S: IndexedFull>(
        &self,
        repo: &Repository<S>,
        path: &Path,
    ) -> RusticResult<Vec<Node>> {
        let result = match self.tree.get_path(path).map_err(|err| {
            RusticError::with_source(
                ErrorKind::Vfs,
                "Failed to get tree at given path `{path}`",
                err,
            )
                .attach_context("path", path.display().to_string())
                .ask_report()
        })? {
            VfsPath::RusticPath(tree_id, path) => {
                let node = repo.node_from_path(*tree_id, &path)?;
                if node.is_dir() {
                    let tree = repo.get_tree(&node.subtree.unwrap())?;
                    tree.nodes
                } else {
                    Vec::new()
                }
            }
            VfsPath::VirtualTree(virtual_tree) => virtual_tree
                .iter()
                .map(|(name, tree)| match tree {
                    VfsTree::Link(target) => Node::new_node(
                        name,
                        NodeType::from_link(Path::new(target)),
                        Metadata::default(),
                    ),
                    VfsTree::File(content) => Node::new_node(
                        name,
                        NodeType::File,
                        Metadata {
                            size: content.len() as u64,
                            ..Metadata::default()
                        },
                    ),
                    _ => Node::new_node(name, NodeType::Dir, Metadata::default()),
                })
                .collect(),
            VfsPath::Link(str) => {
                return Err(RusticError::new(
                    ErrorKind::Vfs,
                    "No directory entries for symlink `{symlink}` found. Is the path valid unicode?",
                )
                    .attach_context("symlink", str.to_string_lossy().to_string()));
            }
            VfsPath::File(_) => {
                return Err(RusticError::new(
                    ErrorKind::Vfs,
                    "Path `{path}` is a virtual file, not a directory",
                )
                    .attach_context("path", path.display().to_string()));
            }
        };
        Ok(result)
    }

    /// Open the file at the given path for reading.
    ///
    /// This works for both real repository files and purely virtual files like
    /// `SUMMARY.txt`. Prefer this over `repo.open_file(&node)` when reading
    /// through the [`Vfs`], as virtual files have no blobs in the repository.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository to open the file from
    /// * `path` - The path of the file within the [`Vfs`]
    ///
    /// # Errors
    ///
    /// * If the path doesn't exist
    /// * If the index for the needed data blobs cannot be read
    pub fn open_file<S: IndexedFull>(
        &self,
        repo: &Repository<S>,
        path: &Path,
    ) -> RusticResult<OpenFile> {
        if let Ok(VfsPath::File(content)) = self.tree.get_path(path) {
            return Ok(OpenFile::from_bytes(content.clone()));
        }
        let node = self.node_from_path(repo, path)?;
        OpenFile::from_node(repo, &node)
    }
}

/// `OpenFile` stores all information needed to access the contents of a file node
#[derive(Debug)]
pub struct OpenFile {
    // The list of blobs
    content: Vec<DataId>,
    startpoints: ContentStartpoints,
    /// In-memory content for purely virtual files (no blobs involved)
    inline: Option<Bytes>,
}

impl OpenFile {
    /// Create an `OpenFile` backed by in-memory bytes (for virtual files)
    pub(crate) fn from_bytes(data: Bytes) -> Self {
        Self {
            content: Vec::new(),
            startpoints: ContentStartpoints(Vec::new()),
            inline: Some(data),
        }
    }

    /// Create an `OpenFile` from a file `Node`
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository to create the `OpenFile` for
    /// * `node` - The `Node` to create the `OpenFile` for
    ///
    /// # Errors
    /// - If the index for the needed data blobs cannot be read
    ///
    /// # Returns
    ///
    /// The created `OpenFile`
    pub(crate) fn from_node<S: IndexedFull>(
        repo: &Repository<S>,
        node: &Node,
    ) -> RusticResult<Self> {
        let content: Vec<_> = node.content.clone().unwrap_or_default();

        let startpoints = ContentStartpoints::from_sizes(content.iter().map(|id| {
            Ok(repo
                .index()
                .get_data(id)
                .ok_or_else(|| {
                    RusticError::new(ErrorKind::Vfs, "blob {blob} is not contained in index")
                        .attach_context("blob", id.to_string())
                })?
                .data_length() as usize)
        }))?;

        Ok(Self {
            content,
            startpoints,
            inline: None,
        })
    }

    /// Read the `OpenFile` at the given `offset` from the `repo`.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository to read the `OpenFile` from
    /// * `offset` - The offset to read the `OpenFile` from
    /// * `length` - The length of the content to read from the `OpenFile`
    ///
    /// # Errors
    ///
    /// - if reading the needed blob(s) from the backend fails
    ///
    /// # Returns
    ///
    /// The read bytes from the given offset and length.
    /// If offset is behind the end of the file, an empty `Bytes` is returned.
    /// If length is too large, the result up to the end of the file is returned.
    pub fn read_at<S: IndexedFull>(
        &self,
        repo: &Repository<S>,
        offset: usize,
        mut length: usize,
    ) -> RusticResult<Bytes> {
        // virtual file: serve straight from memory
        if let Some(data) = &self.inline {
            if offset >= data.len() {
                return Ok(Bytes::new());
            }
            let end = offset.saturating_add(length).min(data.len());
            return Ok(data.slice(offset..end));
        }

        let (mut i, mut offset) = self.startpoints.compute_start(offset);

        let mut result = BytesMut::with_capacity(length);

        // The case of empty node.content is also correctly handled here
        while length > 0 && i < self.content.len() {
            let data = repo.get_blob_cached(&BlobId::from(self.content[i]), BlobType::Data)?;

            if offset > data.len() {
                // we cannot read behind the blob. This only happens if offset is too large to fit in the last blob
                break;
            }

            let to_copy = (data.len() - offset).min(length);
            result.extend_from_slice(&data[offset..offset + to_copy]);
            offset = 0;
            length -= to_copy;
            i += 1;
        }

        Ok(result.into())
    }
}

// helper struct holding blob startpoints of the content
#[derive(Debug)]
struct ContentStartpoints(Vec<usize>);

impl ContentStartpoints {
    fn from_sizes(sizes: impl IntoIterator<Item = RusticResult<usize>>) -> RusticResult<Self> {
        let mut start = 0;
        let mut offsets: Vec<_> = sizes
            .into_iter()
            .map(|size| -> RusticResult<_> {
                let starts_at = start;
                start += size?;
                Ok(starts_at)
            })
            .collect::<RusticResult<_>>()?;

        if !offsets.is_empty() {
            // offsets is assumed to be partitioned, so we add a starts_at:MAX entry
            offsets.push(usize::MAX);
        }
        Ok(Self(offsets))
    }

    // compute the correct blobid and effective offset from a file offset
    fn compute_start(&self, mut offset: usize) -> (usize, usize) {
        if self.0.is_empty() {
            return (0, 0);
        }
        // find the start of relevant blobs => find the largest index such that self.offsets[i] <= offset, but
        // self.offsets[i+1] > offset  (note that a last dummy element with usize::MAX has been added to ensure we always have two partitions)
        // If offsets is non-empty, then offsets[0] = 0, hence partition_point returns an index >=1.
        let i = self.0.partition_point(|o| o <= &offset) - 1;
        offset -= self.0[i];
        (i, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // helper func
    fn startpoints_from_ok_sizes(sizes: impl IntoIterator<Item = usize>) -> ContentStartpoints {
        ContentStartpoints::from_sizes(sizes.into_iter().map(Ok)).unwrap()
    }

    #[test]
    fn content_offsets_empty_sizes() {
        let offsets = startpoints_from_ok_sizes([]);
        assert_eq!(offsets.compute_start(0), (0, 0));
        assert_eq!(offsets.compute_start(42), (0, 0));
    }

    #[test]
    fn content_offsets_size() {
        let offsets = startpoints_from_ok_sizes([15]);
        assert_eq!(offsets.compute_start(0), (0, 0));
        assert_eq!(offsets.compute_start(5), (0, 5));
        assert_eq!(offsets.compute_start(20), (0, 20));
    }
    #[test]
    fn content_offsets_sizes() {
        let offsets = startpoints_from_ok_sizes([15, 24]);
        assert_eq!(offsets.compute_start(0), (0, 0));
        assert_eq!(offsets.compute_start(5), (0, 5));
        assert_eq!(offsets.compute_start(20), (1, 5));
        assert_eq!(offsets.compute_start(42), (1, 27));
    }

    #[test]
    fn human_formatting() {
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(12), "12");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_duration(5.25), "5.2s");
        assert_eq!(human_duration(3725.0), "1h 02m 05s");
    }
}