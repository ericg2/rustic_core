//! `restore` subcommand

use crate::backend::SeekFileOpen;
use derive_setters::Setters;
use log::{debug, error, info, trace, warn};
use smallvec::SmallVec;

use crate::{
    CancelToken, Destination, ReadFileOpen, ReadSourceEntry, WriteFileOpen, WriteHandle,
    backend::{
        FileType, ReadBackend,
        decrypt::DecryptReadBackend,
        node::{Node, NodeType},
    },
    blob::{BlobLocation, BlobLocations},
    error::{ErrorKind, RusticError, RusticResult},
    repofile::packfile::PackId,
    repository::{IndexedFull, IndexedTree, Open, Repository},
};
use bytes::Bytes;
use dashmap::{DashMap, DashSet};
use itertools::Itertools;
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Condvar};
use std::{cmp::Ordering, collections::BTreeMap, path::PathBuf, sync::Mutex};

pub(crate) mod constants {
    /// The maximum number of reader threads to use for restoring.
    pub(crate) const MAX_READER_THREADS_NUM: usize = 20;
}

type Filenames = Vec<PathBuf>;
type RestoreInfo = BTreeMap<(PackId, BlobLocation), SmallVec<[FileLocation; 1]>>;

#[allow(clippy::struct_excessive_bools)]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[derive(Debug, Copy, Clone, Default, Setters)]
#[setters(into)]
#[non_exhaustive]
/// Options for the `restore` command
pub struct RestoreOptions {
    /// Remove all files/dirs in destination which are not contained in snapshot.
    ///
    /// # Warning
    ///
    /// * Use with care, maybe first try this with `--dry-run`?
    #[cfg_attr(feature = "clap", clap(long))]
    pub delete: bool,

    /// Use numeric ids instead of user/group when restoring uid/gui
    #[cfg_attr(feature = "clap", clap(long))]
    pub numeric_id: bool,

    /// Don't restore ownership (user/group)
    #[cfg_attr(feature = "clap", clap(long, conflicts_with = "numeric_id"))]
    pub no_ownership: bool,

    /// Do not scan file contents in the destination (only check hash if available).
    #[cfg_attr(feature = "clap", clap(long, conflicts_with = "verify_existing"))]
    pub no_compare: bool,

    /// Always read and verify existing files (don't trust correct modification time and file size)
    #[cfg_attr(feature = "clap", clap(long))]
    pub verify_existing: bool,
}

#[derive(Default, Debug, Clone, Copy)]
#[non_exhaustive]
/// Statistics for files or directories
pub struct FileDirStats {
    /// Number of files or directories to restore
    pub restore: u64,
    /// Number of files or directories which are unchanged (determined by date, but not verified)
    pub unchanged: u64,
    /// Number of files or directories which are verified and unchanged
    pub verified: u64,
    /// Number of files or directories which are modified
    pub modify: u64,
    /// Number of additional entries
    pub additional: u64,
}

#[derive(Default, Debug, Clone, Copy)]
#[non_exhaustive]
/// Restore statistics
pub struct RestoreStats {
    /// file statistics
    pub files: FileDirStats,
    /// directory statistics
    pub dirs: FileDirStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HardlinkKey {
    device_id: u64,
    inode: u64,
}

/// Restore the repository to the given destination.
///
/// # Type Parameters
///
/// * `P` - The progress bar type
/// * `S` - The type of the indexed tree
///
/// # Arguments
///
/// * `file_infos` - The restore information
/// * `repo` - The repository to restore
/// * `opts` - The restore options
/// * `node_streamer` - The node streamer to use
/// * `dest` - The destination to restore to
///
/// # Errors
///
/// * If the restore failed.
pub(crate) fn restore_repository<S: IndexedTree>(
    file_infos: RestorePlan,
    repo: &Repository<S>,
    opts: RestoreOptions,
    node_streamer: impl Iterator<Item = RusticResult<(PathBuf, Node)>>,
    dest: &impl Destination,
    token: CancelToken,
) -> RusticResult<()> {
    token.check()?;
    repo.warm_up_wait(file_infos.to_packs().into_iter())?;

    token.check()?;
    dest.create_dir_all(Path::new("/"))?; // *** create the root directory here.
    restore_contents(
        repo,
        dest,
        &file_infos.names,
        file_infos.file_lengths,
        file_infos.r,
        file_infos.restore_size,
        token.clone(),
    )?;

    token.check()?;
    let p = repo.progress_spinner("setting metadata...");
    restore_metadata(
        node_streamer,
        &file_infos.hardlink_candidates,
        opts,
        dest,
        token,
    )?;
    p.finish();

    Ok(())
}

/// Collect restore information, scan existing files, create needed dirs and remove superfluous files
///
/// # Type Parameters
///
/// * `P` - The progress bar type.
/// * `S` - The type of the indexed tree.
///
/// # Arguments
///
/// * `repo` - The repository to restore.
/// * `node_streamer` - The node streamer to use.
/// * `dest` - The destination to restore to.
/// * `dry_run` - If true, don't actually restore anything, but only print out what would be done.
///
/// # Errors
///
/// * If a directory could not be created.
/// * If the restore information could not be collected.
#[allow(clippy::too_many_lines)]
pub(crate) fn collect_and_prepare<S, D, N, W, O>(
    repo: &Repository<S>,
    opts: RestoreOptions,
    mut node_streamer: N,
    mut walker: W,
    dest: &D,
    dry_run: bool,
    token: CancelToken,
) -> RusticResult<RestorePlan>
where
    S: IndexedFull,
    D: Destination,
    N: Iterator<Item = RusticResult<(PathBuf, Node)>>,
    W: Iterator<Item = RusticResult<ReadSourceEntry<O>>>,
    O: ReadFileOpen,
{
    token.check()?;

    let p = repo.progress_spinner("collecting file information...");
    dest.create_dir_all(Path::new("/"))?; // *** create the root directory here

    let mut stats = RestoreStats::default();
    let mut restore_infos = RestorePlan::default();
    let mut additional_existing = false;
    let skip_dirs = DashSet::<PathBuf>::new();

    let clean_path = |path: &Path| -> PathBuf {
        Path::new(
            path.to_string_lossy()
                .replace("\\", "/")
                .trim_start_matches("/")
                .trim_end_matches("/"),
        )
        .to_path_buf()
    };

    let next_entry = |walker: &mut W| -> Option<ReadSourceEntry<O>> {
        walker
            .inspect(|r| {
                if let Err(err) = r {
                    error!("Error during collection of files: {err:?}");
                }
            })
            .find_map(|x| {
                if let Some(ret) = x.ok() {
                    if !skip_dirs.iter().any(|x| {
                        let check_a = clean_path(&ret.path);
                        let check_b = clean_path(x.key());
                        check_a.starts_with(check_b)
                    }) {
                        // We need to strip the prefix from the path to avoid "additionals".
                        return Some(ret);
                    }
                }
                None
            })
    };

    let mut process_existing =
        |walker: &mut W, entry: &ReadSourceEntry<O>| -> RusticResult<Option<ReadSourceEntry<O>>> {
            if clean_path(&entry.path) == clean_path(Path::new("/")) {
                // don't process the root dir which should be existing
                return Ok(next_entry(walker));
            }

            debug!("additional {}", entry.path.display());
            let is_dir = entry.node.is_dir();
            if is_dir {
                stats.dirs.additional += 1;
            } else {
                stats.files.additional += 1;
            }
            match (opts.delete, dry_run, is_dir) {
                (true, true, true) => {
                    info!(
                        "would have removed the additional dir: {}",
                        entry.path.display()
                    );
                }
                (true, true, false) => {
                    info!(
                        "would have removed the additional file: {}",
                        entry.path.display()
                    );
                }
                (true, false, true) => {
                    if let Err(err) = dest.remove_dir(&entry.path) {
                        error!("error removing {}: {err}", entry.path.display());
                    }
                }
                (true, false, false) => {
                    if let Err(err) = dest.remove_file(&entry.path) {
                        error!("error removing {}: {err}", entry.path.display());
                    }
                }
                (false, _, _) => {
                    additional_existing = true;
                }
            }

            // don't descend into extra dirs
            if is_dir {
                let _ = skip_dirs.insert(entry.path.clone());
            }
            Ok(next_entry(walker))
        };

    let mut process_node = |path: &PathBuf, node: &Node, exists: bool| -> RusticResult<_> {
        match node.node_type {
            NodeType::Dir => {
                if exists {
                    stats.dirs.modify += 1;
                    trace!("existing dir {}", path.display());
                } else {
                    stats.dirs.restore += 1;
                    debug!("to restore: {}", path.display());
                    if !dry_run {
                        dest.create_dir_all(path)
                            .map_err(|err| {
                                RusticError::with_source(
                                    ErrorKind::InputOutput,
                                    "Failed to create the directory `{path}`. Please check the path and try again.",
                                    err,
                                )
                                    .attach_context("path", path.display().to_string())
                            })?;
                    }
                }
            }
            NodeType::File => {
                if let Some(key) = hardlink_key(node) {
                    match restore_infos.hardlink_candidates.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            trace!("Adding hardlink candidate {}", path.display());
                            _ = entry.insert(path.clone());
                        }
                        std::collections::btree_map::Entry::Occupied(_) => return Ok(()), // this is a hardlink to an existing candidate, will be processed later while setting metadata
                    }
                }
                // collect blobs needed for restoring
                match (
                    exists,
                    restore_infos.add_file(
                        dest,
                        node,
                        path.clone(),
                        repo,
                        opts.verify_existing,
                        opts.no_compare,
                    )?,
                ) {
                    // Note that exists = false and Existing or Verified can happen if the file is changed between scanning the dir
                    // and calling add_file. So we don't care about exists but trust add_file here.
                    (_, AddFileResult::Existing) => {
                        stats.files.unchanged += 1;
                        trace!("identical file: {}", path.display());
                    }
                    (_, AddFileResult::Verified) => {
                        stats.files.verified += 1;
                        trace!("verified identical file: {}", path.display());
                    }
                    // TODO: The differentiation between files to modify and files to create could be done only by add_file
                    // Currently, add_file never returns Modify, but always New, so we differentiate based on exists
                    (true, AddFileResult::Modify) => {
                        stats.files.modify += 1;
                        debug!("to modify: {}", path.display());
                    }
                    (false, AddFileResult::Modify) => {
                        stats.files.restore += 1;
                        debug!("to restore: {}", path.display());
                    }
                }
            }
            _ => {} // nothing to do for symlink, device, etc.
        }
        Ok(())
    };

    let mut next_dst = next_entry(&mut walker);
    let mut next_node = node_streamer.next().transpose()?;
    loop {
        // Check at the top of every iteration so we stop cleanly between
        // entries rather than mid-way through processing one.
        token.check()?;

        match (&next_dst, &next_node) {
            (None, None) => break,

            (Some(destination), None) => {
                next_dst = process_existing(&mut walker, destination)?;
            }
            (Some(destination), Some((path, node))) => {
                let path_a = clean_path(&destination.path);
                let path_b = clean_path(path);
                trace!("comparing {:?} with {:?}", &path_a, &path_b);
                match path_a.cmp(&path_b) {
                    Ordering::Less => {
                        next_dst = process_existing(&mut walker, destination)?;
                    }
                    Ordering::Equal => {
                        // process existing node
                        if (node.is_dir() && !destination.node.is_dir())
                            || (node.is_file() && !destination.node.is_file())
                            || node.is_special()
                        {
                            // if types do not match, first remove the existing file
                            next_dst = process_existing(&mut walker, destination)?;
                        } else {
                            next_dst = next_entry(&mut walker);
                        }
                        process_node(path, node, true)?;
                        next_node = node_streamer.next().transpose()?;
                    }
                    Ordering::Greater => {
                        process_node(path, node, false)?;
                        next_node = node_streamer.next().transpose()?;
                    }
                }
            }
            (None, Some((path, node))) => {
                process_node(path, node, false)?;
                next_node = node_streamer.next().transpose()?;
            }
        }
    }

    if additional_existing {
        warn!("Note: additional entries exist in destination");
    }

    restore_infos.stats = stats;
    p.finish();

    Ok(restore_infos)
}

/// Restore the metadata of the files and directories.
///
/// # Arguments
///
/// * `node_streamer` - The node streamer to use
/// * `opts` - The restore options to use
/// * `dest` - The destination to restore to
///
/// # Errors
///
/// * If the restore failed.
fn restore_metadata(
    mut node_streamer: impl Iterator<Item = RusticResult<(PathBuf, Node)>>,
    hardlink_candidates: &BTreeMap<HardlinkKey, PathBuf>,
    opts: RestoreOptions,
    dest: &impl Destination,
    token: CancelToken,
) -> RusticResult<()> {
    let mut dir_stack: Vec<(PathBuf, Node)> = Vec::new();
    while let Some((path, node)) = node_streamer.next().transpose()? {
        token.check()?;

        if dest.can_hard_link() {
            // Create hardlink directly, if this is one.
            if let Some(key) = hardlink_key(&node)
                && let Some(canonical) = hardlink_candidates.get(&key)
                && canonical != &path
            {
                debug!(
                    "restoring hardlink {} -> {}",
                    path.display(),
                    canonical.display()
                );
                dest.hard_link(canonical, &path).map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::InputOutput,
                        "Failed to recreate the hardlink `{path}` from `{canonical}`.",
                        err,
                    )
                    .attach_context("path", path.display().to_string())
                    .attach_context("canonical", canonical.display().to_string())
                })?;
            }
        }

        match node.node_type {
            NodeType::Dir => {
                // set metadata for all non-parent paths in stack
                while let Some((stackpath, _)) = dir_stack.last() {
                    if path.starts_with(stackpath) {
                        break;
                    }
                    let (path, node) = dir_stack.pop().unwrap();
                    dest.set_restore_metadata(&path, &node, &opts)?;
                }
                // push current path to the stack
                dir_stack.push((path, node));
            }
            _ => dest.set_restore_metadata(&path, &node, &opts)?,
        }
    }

    // empty dir stack and set metadata
    for (path, node) in dir_stack.into_iter().rev() {
        dest.set_restore_metadata(&path, &node, &opts)?;
    }

    Ok(())
}

fn hardlink_key(node: &Node) -> Option<HardlinkKey> {
    (matches!(node.node_type, NodeType::File)
        && node.meta.links > 1
        && node.meta.device_id != 0
        && node.meta.inode != 0)
        .then_some(HardlinkKey {
            device_id: node.meta.device_id,
            inode: node.meta.inode,
        })
}

struct PackInfo {
    pack_id: PackId,
    from_file: Option<(usize, u64, u32)>,
    locations: BlobLocations<SmallVec<[(usize, u64); 1]>>,
}

impl PackInfo {
    #[allow(clippy::result_large_err)]
    /// coalesce two `PackInfo` if possible
    fn coalesce(self, other: Self) -> Result<Self, (Self, Self)> {
        if self.pack_id == other.pack_id // if the pack is identical
            && self.from_file.is_none() // and we don't read from a present file
            // and the blobs can be coalesced
            && self.locations.can_coalesce(&other.locations)
        {
            Ok(Self {
                pack_id: self.pack_id,
                from_file: self.from_file,
                locations: self.locations.append(other.locations),
            })
        } else {
            Err((self, other))
        }
    }
}

struct AppendState {
    cursor: u64,
    handle: Option<Box<dyn WriteHandle>>,
}

#[allow(clippy::too_many_lines)]
fn restore_contents<S: Open>(
    repo: &Repository<S>,
    dest: &impl Destination,
    filenames: &Filenames,
    file_lengths: Vec<u64>,
    restore_info: RestoreInfo,
    restore_size: u64,
    token: CancelToken,
) -> RusticResult<()> {
    token.check()?;

    let be = repo.dbe();
    let num_files = file_lengths.len();

    // For random-write: create empty files now; non-empty files
    // are lazily allocated on first write via the sizes mutex.
    // For append: truncate ALL files to 0 now so appends start from a clean
    // slate (no stale content, no pre-allocated zeros causing over-length output).
    for (i, size) in file_lengths.iter().enumerate() {
        if *size == 0 {
            let path = &filenames[i];
            if let Some(parent) = path.parent() {
                dest.create_dir_all(parent)?;
            }
            dest.set_length(path, 0).map_err(|err| {
                RusticError::with_source(
                    ErrorKind::InputOutput,
                    "Failed to set the length of the file `{path}`. Please check the path and try again.",
                    err,
                )
                    .attach_context("path", path.display().to_string())
            })?;
        }
    }

    // Random-write path: tracks which files still need to be allocated
    // (set to their full size) before the first write_at.
    let sizes = &Mutex::new(file_lengths);

    // Append path: per-file cursor + condvar so concurrent threads always
    // append in offset order. The open write handle lives here too, so a
    // single lock covers both — no DashMap needed.
    let write_state: Vec<(Mutex<AppendState>, Condvar)> = (0..num_files)
        .map(|_| {
            (
                Mutex::new(AppendState {
                    cursor: 0,
                    handle: None,
                }),
                Condvar::new(),
            )
        })
        .collect();

    let write_state = &write_state;
    let p = repo.progress_bytes("restoring file contents...");
    p.set_length(restore_size);

    let p = &p;

    // Borrow the token so rayon scoped closures can reference it without
    // moving it — the scope guarantees all threads finish before we return.
    let token = &token;

    let packs: Vec<_> = restore_info
        .into_iter()
        .map(|((pack_id, bl), fls)| {
            let from_file = fls
                .iter()
                .find(|fl| fl.matches)
                .map(|fl| (fl.file_idx, fl.file_start, bl.data_length()));

            let name_dests = fls
                .iter()
                .filter(|fl| !fl.matches)
                .map(|fl| (fl.file_idx, fl.file_start))
                .collect();

            PackInfo {
                pack_id,
                from_file,
                locations: BlobLocations::from_blob_location(bl, name_dests),
            }
        })
        .coalesce(PackInfo::coalesce)
        .collect();

    let threads = constants::MAX_READER_THREADS_NUM;
    let pool = ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|err| {
            RusticError::with_source(
                ErrorKind::Internal,
                "Failed to create the thread pool with `{num_threads}` threads. Please try again.",
                err,
            )
            .attach_context("num_threads", threads.to_string())
        })?;

    pool.in_place_scope(|s| {
        for PackInfo {
            pack_id,
            from_file,
            locations:
                BlobLocations {
                    offset,
                    length,
                    blobs,
                },
        } in packs
        {
            if blobs.is_empty() {
                continue;
            }
            // Stop dispatching new pack reads once cancelled. Already-queued
            // rayon tasks will still run (rayon doesn't support cancellation),
            // but they each check the token themselves and exit immediately.
            if token.is_cancelled() {
                break;
            }
            s.spawn(move |s1| {
                if token.is_cancelled() {
                    return;
                }

                let read_data = match (dest.can_random_write(), &from_file) {
                    (true, Some((file_idx, offset_file, length_file))) => {
                        let length: u64 = (*length_file).into();
                        let offset: u64 = *offset_file;
                        let path = &filenames[*file_idx];
                        let mut buf = vec![0; length as usize];
                        let mut file = dest.get_reader(path).and_then(|x| x.open()).unwrap();
                        let _ = file.seek(SeekFrom::Start(offset)).unwrap();
                        file.read_exact(&mut buf).unwrap();
                        Bytes::from(buf)
                    }
                    _ => be
                        .read_partial(FileType::Pack, &pack_id, false, offset, length)
                        .unwrap(),
                };

                for (bl, name_dests) in blobs {
                    if token.is_cancelled() {
                        return;
                    }

                    let size: u64 = bl.data_length().into();
                    let data = if dest.can_random_write() && from_file.is_some() {
                        read_data.clone()
                    } else {
                        let start = usize::try_from(bl.offset - offset)
                            .expect("bl.offset - offset overflows usize");
                        let end = usize::try_from(bl.offset + bl.length - offset)
                            .expect("bl.offset + bl.length - offset overflows usize");
                        be.read_encrypted_from_partial(
                            &read_data[start..end],
                            bl.uncompressed_length,
                        )
                        .unwrap()
                    };

                    for (file_idx, start) in name_dests {
                        if token.is_cancelled() {
                            return;
                        }

                        let data = data.clone();
                        s1.spawn(move |_| {
                            if token.is_cancelled() {
                                return;
                            }

                            let path = &filenames[file_idx];
                            if dest.can_random_write() {
                                // Lazily allocate the file to its full size on the first write.
                                let mut sizes_guard = sizes.lock().unwrap();
                                let filesize = sizes_guard[file_idx];
                                if filesize > 0 {
                                    if let Some(parent) = path.parent() {
                                        dest.create_dir_all(parent).unwrap();
                                    }
                                    dest.set_length(path, filesize).unwrap();
                                    sizes_guard[file_idx] = 0;
                                }
                                drop(sizes_guard);
                                dest.write_at(path, start, &data).unwrap();
                            } else {
                                // Block until it is our turn to append, so writes
                                // are contiguous. Data waits on this thread's stack —
                                // nothing is heap-buffered while blocking.
                                let (state_mutex, condvar) = &write_state[file_idx];
                                let mut state = condvar
                                    .wait_while(state_mutex.lock().unwrap(), |s| s.cursor != start)
                                    .unwrap();
                                assert_eq!(
                                    state.cursor, start,
                                    "non-contiguous write to {path:?}: expected {}, got {start}",
                                    state.cursor,
                                );
                                let handle = state.handle.get_or_insert_with(|| {
                                    debug!("Opening write handle to {:?}", path);
                                    Box::new(dest.get_writer(path).unwrap().open_replace().unwrap())
                                });
                                handle.write_all(&data).unwrap();
                                handle.flush().unwrap();
                                state.cursor += data.len() as u64;
                                condvar.notify_all();
                            }
                            p.inc(size);
                        });
                    }
                }
            });
        }
    });

    // Finally, close all handles to ensure files are written.
    for (state_mutex, _) in write_state {
        let mut state = state_mutex.lock().unwrap();
        if let Some(mut handle) = state.handle.take() {
            handle.close()?;
        }
    }

    p.finish();
    Ok(())
}

/// Information about what will be restored.
///
/// Struct that contains information of file contents grouped by
/// 1) pack ID,
/// 2) blob within this pack
/// 3) the actual files and position of this blob within those
/// 4) Statistical information
#[derive(Debug, Default)]
pub struct RestorePlan {
    /// The names of the files to restore
    names: Filenames,
    /// The length of the files to restore
    file_lengths: Vec<u64>,
    /// The restore information
    r: RestoreInfo,
    /// candidates for hardlinks
    hardlink_candidates: BTreeMap<HardlinkKey, PathBuf>,
    /// The total restore size
    pub restore_size: u64,
    /// The total size of matched content, i.e. content with needs no restore.
    pub matched_size: u64,
    /// Statistics about the restore.
    pub stats: RestoreStats,
}

/// [`FileLocation`] contains information about a file within a blob
#[derive(Debug)]
struct FileLocation {
    // TODO: The index of the file within ... ?
    file_idx: usize,
    /// The start of the file within the blob
    file_start: u64,
    /// Whether the file matches the blob
    ///
    /// This indicates that the file exists and these contents are already correct.
    matches: bool,
}

/// [`AddFileResult`] indicates the result of adding a file to [`FileLocation`]
// TODO: Add documentation!
enum AddFileResult {
    Existing,
    Verified,
    Modify,
}

impl RestorePlan {
    /// Add the file to [`FileLocation`] using `index` to get blob information.
    ///
    /// # Type Parameters
    ///
    /// * `P` - The progress bar type.
    /// * `S` - The type of the indexed tree.
    ///
    /// # Arguments
    ///
    /// * `dest` - The destination to restore to.
    /// * `file` - The file to add.
    /// * `name` - The name of the file.
    /// * `repo` - The repository to restore.
    /// * `ignore_mtime` - If true, ignore the modification time of the file.
    /// * `no_compare` - If true, do not scan the source files.
    ///
    /// # Errors
    ///
    /// * If the file could not be added.
    fn add_file<S: IndexedFull>(
        &mut self,
        dest: &impl Destination,
        file: &Node,
        name: PathBuf,
        repo: &Repository<S>,
        ignore_mtime: bool,
        no_read: bool,
    ) -> RusticResult<AddFileResult> {
        let existing_file = dest
            .get_existing(&name)?
            .filter(|meta| meta.size == file.meta.size);

        // Empty files which exists with correct size should always return Ok(Existing)!
        if file.meta.size == 0 && existing_file.is_some() {
            return Ok(AddFileResult::Existing);
        }

        if !ignore_mtime {
            if let Some(ref meta) = existing_file {
                if meta.size == file.meta.size && meta.mtime == file.meta.mtime {
                    // File exists with fitting mtime => we suspect this file is ok!
                    debug!(
                        "file {} exists with suitable size and mtime, accepting it!",
                        name.display()
                    );
                    self.matched_size += file.meta.size;
                    return Ok(AddFileResult::Existing);
                }
            }
        }

        // If no_read is set, we never read the destination's content - some object
        // storages charge enough for downloads that it's cheaper to just re-upload.
        // In that case every blob below is treated as unverified; the mtime check
        // above is the only way this file can come back as Existing/Verified.
        let mut open = if !no_read && existing_file.is_some() {
            Some(dest.get_reader(&name)?.open()?)
        } else {
            None
        };

        let file_idx = self.names.len();
        self.names.push(name.clone());
        let mut file_pos: u64 = 0;
        let mut has_unmatched = false;
        let mut matched_size = 0;
        let mut restore_size = 0;

        // On append-only destinations, one mismatch means the whole file gets
        // replaced, so any blobs already marked "matched" need flipping. We
        // remember them here just in case; this stays empty (and unused) on
        // random-write destinations, and gets drained+cleared the moment the
        // first mismatch is found, so it's never a second full pass.
        let mut tentatively_matched = Vec::new();

        for id in file.content.iter().flatten() {
            let ie = repo.get_index_entry(id)?;
            let bl = ie.location;
            let length: u64 = bl.data_length().into();
            let key = (ie.pack, bl);

            // Append-only backends (like OpenDAL) only allow a whole file replacement.
            // As a result, we can skip reading from `dest` as soon as we know the file
            // is different - since it needs to get replaced anyway.
            let should_read = !has_unmatched || dest.can_random_write();
            let matches = should_read
                && open
                    .as_mut()
                    .is_some_and(|open| id.blob_matches_reader(length, open));

            if matches {
                matched_size += length;
                if !dest.can_random_write() {
                    tentatively_matched.push((key.clone(), length));
                }
            } else {
                if !has_unmatched && !dest.can_random_write() {
                    // First mismatch on an append-only destination: flip
                    // everything we tentatively counted as matched so far.
                    for (k, len) in tentatively_matched.drain(..) {
                        if let Some(last) = self.r.get_mut(&k).and_then(|v| v.last_mut()) {
                            last.matches = false;
                        }
                        matched_size -= len;
                        restore_size += len;
                    }
                }
                restore_size += length;
                has_unmatched = true;
            }

            self.r.entry(key).or_default().push(FileLocation {
                file_idx,
                file_start: file_pos,
                matches,
            });

            file_pos += length;
        }

        self.restore_size += restore_size;
        self.matched_size += matched_size;
        self.file_lengths.push(file_pos);

        if !has_unmatched && existing_file.is_some() {
            Ok(AddFileResult::Verified)
        } else {
            Ok(AddFileResult::Modify)
        }
    }

    /// Get a list of all pack files needed to perform the restore
    ///
    /// This can be used e.g. to warm-up those pack files before doing the actual restore.
    #[must_use]
    pub fn to_packs(&self) -> Vec<PackId> {
        self.r
            .iter()
            // filter out packs which we need
            .filter(|(_, fls)| fls.iter().all(|fl| !fl.matches))
            .map(|((pack, _), _)| *pack)
            .dedup()
            .collect()
    }
}
