//! `restore` subcommand

use derive_setters::Setters;
use log::{debug, error, info, trace, warn};
use smallvec::SmallVec;

use dashmap::DashSet;
use itertools::Itertools;
use rayon::ThreadPoolBuilder;
use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Condvar};
use std::{cmp::Ordering, collections::BTreeMap, path::PathBuf, sync::Mutex};

use crate::{
    Destination, ReadSourceEntry, ReadFileOpen,
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
) -> RusticResult<()> {
    repo.warm_up_wait(file_infos.to_packs().into_iter())?;
    restore_contents(
        repo,
        dest,
        &file_infos.names,
        file_infos.file_lengths,
        file_infos.r,
        file_infos.restore_size,
    )?;

    let p = repo.progress_spinner("setting metadata...");
    restore_metadata(node_streamer, &file_infos.hardlink_candidates, opts, dest)?;
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
) -> RusticResult<RestorePlan>
where
    S: IndexedFull,
    D: Destination,
    N: Iterator<Item = RusticResult<(PathBuf, Node)>>,
    W: Iterator<Item = RusticResult<ReadSourceEntry<O>>>,
    O: ReadFileOpen,
{
    let p = repo.progress_spinner("collecting file information...");
    let dest_path = dest.path(Path::new(""));

    let mut stats = RestoreStats::default();
    let mut restore_infos = RestorePlan::default();
    let mut additional_existing = false;
    let skip_dirs = DashSet::<PathBuf>::new();

    let next_entry = |walker: &mut W| -> Option<ReadSourceEntry<O>> {
        walker
            .inspect(|r| {
                if let Err(err) = r {
                    error!("Error during collection of files: {err:?}");
                }
            })
            .find_map(|x| {
                if let Some(ret) = x.ok() {
                    if !skip_dirs.iter().any(|x| ret.path.starts_with(x.key())) {
                        return Some(ret);
                    }
                }
                None
            })
    };

    let mut process_existing =
        |walker: &mut W, entry: &ReadSourceEntry<O>| -> RusticResult<Option<ReadSourceEntry<O>>> {
            if entry.path == dest_path {
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
                skip_dirs.insert(entry.path.clone());
                //walker.skip_current_dir();
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
                    restore_infos.add_file(dest, node, path.clone(), repo, opts.verify_existing)?,
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
        match (&next_dst, &next_node) {
            (None, None) => break,

            (Some(destination), None) => {
                next_dst = process_existing(&mut walker, destination)?;
            }
            (Some(destination), Some((path, node))) => {
                match destination.path.cmp(&dest.path(path)) {
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
) -> RusticResult<()> {
    let mut dir_stack: Vec<(PathBuf, Node)> = Vec::new();
    while let Some((path, node)) = node_streamer.next().transpose()? {
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
#[allow(clippy::too_many_lines)]
fn restore_contents<S: Open>(
    repo: &Repository<S>,
    dest: &impl Destination,
    filenames: &Filenames,
    file_lengths: Vec<u64>,
    restore_info: RestoreInfo,
    restore_size: u64,
) -> RusticResult<()> {
    let be = repo.dbe();
    let num_files = file_lengths.len();

    // For random-write: create empty files now, non-empty files are lazily
    // allocated on first write via the sizes mutex.
    // For append: truncate ALL files to 0 now so appends start from a clean
    // slate (no stale content, no pre-allocated zeros that would cause
    // over-length output).
    for (i, size) in file_lengths.iter().enumerate() {
        if *size == 0 || !dest.can_random_write() {
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

    // Append path: per-file cursor + condvar so that concurrent threads
    // always append in offset order. Data waits on the calling thread's
    // stack — nothing is heap-buffered while blocking.
    let write_cursors: Vec<(Mutex<u64>, Condvar)> = (0..num_files)
        .map(|_| (Mutex::new(0u64), Condvar::new()))
        .collect();
    let write_cursors = &write_cursors;

    let p = repo.progress_bytes("restoring file contents...");
    p.set_length(restore_size);

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
        // optimize reading from backend by reading many blobs in a row
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
            let p = &p;
            if !blobs.is_empty() {
                // TODO: error handling!
                s.spawn(move |s1| {
                    let read_data = match (dest.can_random_write(), &from_file) {
                        (true, Some((file_idx, offset_file, length_file))) => dest
                            .read_exact(
                                &filenames[*file_idx],
                                *offset_file,
                                (*length_file).into(),
                            )
                            .unwrap(),

                        _ => be
                            .read_partial(FileType::Pack, &pack_id, false, offset, length)
                            .unwrap(),
                    };

                    // save into needed files in parallel
                    for (bl, name_dests) in blobs {
                        let size = bl.data_length().into();
                        let data = if dest.can_random_write() && from_file.is_some() {
                            read_data.clone()
                        } else {
                            let start = usize::try_from(bl.offset - offset)
                                .expect("convert from u32 to usize should not fail!");
                            let end = usize::try_from(bl.offset + bl.length - offset)
                                .expect("convert from u32 to usize should not fail!");
                            be.read_encrypted_from_partial(
                                &read_data[start..end],
                                bl.uncompressed_length,
                            )
                                .unwrap()
                        };
                        for (file_idx, start) in name_dests {
                            let data = data.clone();
                            s1.spawn(move |_| {
                                let path = &filenames[file_idx];
                                if dest.can_random_write() {
                                    // Allocate file if it is not yet allocated
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
                                    // are contiguous. Data waits on this thread's stack.
                                    let (cursor_mutex, condvar) = &write_cursors[file_idx];
                                    let mut cursor = condvar
                                        .wait_while(
                                            cursor_mutex.lock().unwrap(),
                                            |c| *c != start,
                                        )
                                        .unwrap();
                                    assert_eq!(
                                        *cursor, start,
                                        "non-contiguous write to {path:?}: expected offset {}, got {start}",
                                        *cursor,
                                    );
                                    dest.append(path, &data).unwrap();
                                    *cursor += data.len() as u64;
                                    condvar.notify_all();
                                }
                                p.inc(size);
                            });
                        }
                    }
                });
            }
        }
    });

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

        let file_idx = self.names.len();
        self.names.push(name.clone());
        let mut file_pos = 0;
        let mut has_unmatched = false;
        for id in file.content.iter().flatten() {
            let ie = repo.get_index_entry(id)?;
            let bl = ie.location;
            let length: u64 = bl.data_length().into();

            let mut matches = false;
            let blob_location = self.r.entry((ie.pack, bl)).or_default();
            blob_location.push(FileLocation {
                file_idx,
                file_start: file_pos,
                matches,
            });

            // We can skip reading from `dest` as soon as we know the file is different.
            let should_read = !has_unmatched || dest.can_random_write();
            if should_read && existing_file.is_some() {
                let mut data = Cursor::new(dest.read_exact(&name, file_pos, length)?);
                matches = id.blob_matches_reader(length, &mut data);
            }

            if !matches {
                has_unmatched = true;
            }

            self.restore_size += length;
            file_pos += length;
        }

        self.file_lengths.push(file_pos);

        // TODO: FIXME: optimize!
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
