use std::fs;
#[cfg(not(windows))]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Result;
use rustic_backend::local::LocalSource;
use rustic_core::{
    BackupOptions, CancelToken, IndexedFullStatus, IndexedIdsStatus, LsOptions, Repository,
    RestoreOptions, repofile::SnapshotFile,
};

use rstest::rstest;
use tempfile::tempdir;

use super::{RepoOpen, TestSource, set_up_repo, tar_gz_testdata};

/// Backup `source` under `as_path` and return the resulting snapshot.
fn backup_source(
    repo: &Repository<IndexedIdsStatus>,
    source: &LocalSource,
    as_path: &str,
) -> Result<SnapshotFile> {
    let opts = BackupOptions::default().as_path(PathBuf::from(as_path));
    Ok(repo
        .backup(SnapshotFile::default())
        .options(opts)
        .add_source(source)
        .run()?)
}

/// Restore the snapshot identified by `snapshot` (an id, id prefix, or
/// `latest`) into `dest`.
///
/// The whole snapshot is restored below `dest` at `/`, preserving the
/// snapshot's internal path structure.
fn restore_snapshot(
    repo: &Repository<IndexedFullStatus>,
    dest: &LocalSource,
    snapshot: &str,
    opts: RestoreOptions,
) -> Result<()> {
    let node = repo.node_from_snapshot_path(snapshot, |_| true)?;
    let ls_opts = LsOptions::default();
    let ls = repo.ls(&node, &ls_opts)?;
    let plan = repo.prepare_restore(
        &opts,
        ls.clone(),
        dest,
        Path::new("/"),
        false,
        CancelToken::new(),
    )?;
    repo.restore(plan, &opts, ls, dest, CancelToken::new())?;
    Ok(())
}

#[rstest]
#[cfg(not(windows))]
fn test_restore_preserves_hardlinks(
    tar_gz_testdata: Result<TestSource>,
    set_up_repo: Result<RepoOpen>,
) -> Result<()> {
    let (source, repo) = (tar_gz_testdata?, set_up_repo?.to_indexed_ids()?);

    let opts = BackupOptions::default().as_path(PathBuf::from_str("test")?);
    let src = LocalSource::new(source.path());
    let _snapshot = repo
        .backup(SnapshotFile::default())
        .options(opts)
        .add_source(&src)
        .run()?;

    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    let ls_opts = LsOptions::default();
    let ls = repo.ls(&node, &ls_opts)?;

    let restore_dir = tempdir()?;
    let dest = LocalSource::new(restore_dir.path());
    let restore_opts = RestoreOptions::default();
    let plan = repo.prepare_restore(
        &restore_opts,
        ls.clone(),
        &dest,
        Path::new("/"),
        false,
        CancelToken::new(),
    )?;
    repo.restore(plan, &restore_opts, ls, &dest, CancelToken::new())?;

    let hardlink = restore_dir.path().join("test/0/tests/testfile-hardlink");
    let linked = restore_dir.path().join("test/0/tests/testfile");
    let symlink = restore_dir.path().join("test/0/tests/testfile-symlink");

    let hardlink_meta = fs::metadata(&hardlink)?;
    let linked_meta = fs::metadata(&linked)?;
    assert_eq!(hardlink_meta.dev(), linked_meta.dev());
    assert_eq!(hardlink_meta.ino(), linked_meta.ino());
    assert_eq!(hardlink_meta.nlink(), 2);
    assert_eq!(linked_meta.nlink(), 2);
    assert_eq!(fs::read_to_string(&hardlink)?, fs::read_to_string(&linked)?);
    assert_eq!(fs::read_link(&symlink)?, PathBuf::from("testfile"));

    Ok(())
}

#[cfg(not(windows))]
#[rstest]
fn test_backup_restore_round_trip_preserves_contents_links_and_modes(
    set_up_repo: Result<RepoOpen>,
) -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let repo = set_up_repo?.to_indexed_ids()?;
    let source_dir = tempdir()?;
    let source_root = source_dir.path();

    fs::write(source_root.join("file.txt"), "hello")?;
    fs::create_dir_all(source_root.join("dir/sub"))?;
    fs::write(source_root.join("dir/nested.txt"), "nested")?;
    fs::create_dir(source_root.join("empty-dir"))?;
    fs::write(source_root.join("executable.sh"), "#!/bin/sh\n")?;
    fs::set_permissions(
        source_root.join("executable.sh"),
        fs::Permissions::from_mode(0o751),
    )?;
    symlink("file.txt", source_root.join("link.txt"))?;
    fs::write(source_root.join("hard-a"), "hardlinked")?;
    fs::hard_link(source_root.join("hard-a"), source_root.join("hard-b"))?;

    let src = LocalSource::new(source_root);
    let _snapshot = backup_source(&repo, &src, "data")?;

    let repo = repo.to_indexed()?;
    let restore_dir = tempdir()?;
    let dest = LocalSource::new(restore_dir.path());
    restore_snapshot(&repo, &dest, "latest", RestoreOptions::default())?;

    let root = restore_dir.path().join("data");
    assert_eq!(fs::read_to_string(root.join("file.txt"))?, "hello");
    assert_eq!(fs::read_to_string(root.join("dir/nested.txt"))?, "nested");
    assert!(root.join("dir/sub").is_dir());
    assert!(root.join("empty-dir").is_dir());

    let executable = fs::metadata(root.join("executable.sh"))?;
    assert_eq!(executable.permissions().mode() & 0o777, 0o751);

    assert_eq!(
        fs::read_link(root.join("link.txt"))?,
        PathBuf::from("file.txt")
    );

    let hard_a = fs::metadata(root.join("hard-a"))?;
    let hard_b = fs::metadata(root.join("hard-b"))?;
    assert_eq!(hard_a.ino(), hard_b.ino());
    assert_eq!(hard_a.nlink(), 2);
    assert_eq!(
        fs::read_to_string(root.join("hard-a"))?,
        fs::read_to_string(root.join("hard-b"))?
    );

    Ok(())
}

#[rstest]
fn test_restore_delete_removes_extra_files(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let source_dir = tempdir()?;
    fs::write(source_dir.path().join("keep.txt"), "keep")?;
    fs::create_dir(source_dir.path().join("dir"))?;
    fs::write(source_dir.path().join("dir/keep.txt"), "keep")?;

    let src = LocalSource::new(source_dir.path());
    let _snapshot = backup_source(&repo, &src, "data")?;

    let repo = repo.to_indexed()?;
    let restore_dir = tempdir()?;
    let dest = LocalSource::new(restore_dir.path());
    restore_snapshot(&repo, &dest, "latest", RestoreOptions::default())?;

    let extra_nested = restore_dir.path().join("data/extra.txt");
    let extra_root = restore_dir.path().join("extra-root.txt");
    fs::write(&extra_nested, "extra")?;
    fs::write(&extra_root, "extra")?;

    let opts = RestoreOptions::default().delete(true);
    restore_snapshot(&repo, &dest, "latest", opts)?;

    assert!(restore_dir.path().join("data/keep.txt").exists());
    assert!(restore_dir.path().join("data/dir/keep.txt").exists());
    assert!(
        !extra_nested.exists(),
        "extra file below snapshot root was kept"
    );
    assert!(
        !extra_root.exists(),
        "extra file at destination root was kept"
    );

    Ok(())
}

#[rstest]
fn test_restore_replaces_modified_files_and_older_snapshots(
    set_up_repo: Result<RepoOpen>,
) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let source_dir = tempdir()?;
    let file = source_dir.path().join("file.txt");
    fs::write(&file, "first")?;

    let src = LocalSource::new(source_dir.path());
    let first = backup_source(&repo, &src, "data")?;

    let repo = repo.to_indexed_ids()?;
    fs::write(&file, "second version")?;
    let second = repo
        .backup(SnapshotFile::default())
        .options(BackupOptions::default().as_path(PathBuf::from("data")))
        .add_source(&src)
        .run()?;
    assert_ne!(first.tree, second.tree);

    let repo = repo.to_indexed()?;
    let restore_dir = tempdir()?;
    let dest = LocalSource::new(restore_dir.path());
    restore_snapshot(&repo, &dest, "latest", RestoreOptions::default())?;
    let restored = restore_dir.path().join("data/file.txt");
    assert_eq!(fs::read_to_string(&restored)?, "second version");

    // Simulate local modification. The different size guarantees that restore
    // cannot skip the file based on its timestamp alone.
    fs::write(&restored, "locally modified, much longer")?;
    restore_snapshot(
        &repo,
        &dest,
        &first.id.to_hex(),
        RestoreOptions::default(),
    )?;
    assert_eq!(fs::read_to_string(&restored)?, "first");

    Ok(())
}

#[rstest]
fn test_restore_dry_run_does_not_write_to_destination(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let source_dir = tempdir()?;
    fs::write(source_dir.path().join("file.txt"), "some content")?;
    fs::create_dir(source_dir.path().join("dir"))?;
    fs::write(source_dir.path().join("dir/nested.txt"), "more content")?;

    let src = LocalSource::new(source_dir.path());
    let _snapshot = backup_source(&repo, &src, "data")?;

    let repo = repo.to_indexed()?;
    let restore_dir = tempdir()?;
    let dest = LocalSource::new(restore_dir.path());

    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    let ls_opts = LsOptions::default();
    let ls = repo.ls(&node, &ls_opts)?;
    let opts = RestoreOptions::default();
    let plan = repo.prepare_restore(&opts, ls, &dest, Path::new("/"), true, CancelToken::new())?;

    assert!(plan.restore_size > 0);
    assert_eq!(
        fs::read_dir(restore_dir.path())?.count(),
        0,
        "dry-run restore created files in the destination"
    );

    Ok(())
}
