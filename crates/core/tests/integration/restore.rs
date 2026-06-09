use std::{fs, path::PathBuf, str::FromStr};

#[cfg(not(windows))]
use std::os::unix::fs::MetadataExt;

use super::{RepoOpen, TestSource, set_up_repo, tar_gz_testdata};
use anyhow::Result;
use pretty_assertions::assert_eq;
use rstest::rstest;
use rustic_backend::BackendOptions;
use rustic_backend::local::{LocalDestination, LocalSource};
use rustic_core::{BackupOptions, ConfigOptions, Credentials, Destination, KeyOptions, LsOptions, Repository, RepositoryBackends, RepositoryOptions, RestoreOptions, repofile::SnapshotFile, SnapshotOptions};
use tempfile::tempdir;

#[test]
fn test_restore_local() -> Result<()> {
    let repo_be = BackendOptions::default()
        .repository("C:\\Users\\Eric\\Documents\\test-repo-6-6")
        .to_backends()?;

    let repo_opts = RepositoryOptions::default();
    let repo_creds = Credentials::password("Rugratse124!");
    let repo_key = KeyOptions::default();
    let repo_config = ConfigOptions::default();
    let repo = Repository::new(&repo_opts, &repo_be)?
        .init(&repo_creds, &repo_key, &repo_config)?
        .to_indexed_ids()?;

    let opts = BackupOptions::default();
    let src = LocalSource::new("C:\\Users\\Eric\\Documents\\test-6-6-26");
    let snap = SnapshotOptions::default()
        .add_tags("tag1,tag2")?
        .to_snapshot()?;

    let snap = repo.backup(&opts, &src, snap)?;

    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    let ls_opts = LsOptions::default();
    let ls = repo.ls(&node, &ls_opts)?;

    let dest = LocalDestination::new("C:\\Users\\Eric\\Documents\\restore-6-6-26");
    let restore_opts = RestoreOptions::default();
    let plan = repo.prepare_restore(&restore_opts, ls.clone(), &dest, false)?;
    repo.restore(plan, &restore_opts, ls, &dest)?;

    Ok(())
}

// #[rstest]
// fn test_restore_local(tar_gz_testdata: Result<TestSource>, set_up_repo: Result<RepoOpen>) -> Result<()> {
//     let (source, repo) = (tar_gz_testdata?, set_up_repo?.to_indexed_ids()?);
//     let opts = BackupOptions::default();
//     let src = LocalSource::new(&source.path_list());
//     let snap = repo.backup(&opts, &src, SnapshotFile::default())?;
//
//     let repo = repo.to_indexed()?;
//     let node = repo.node_from_snapshot_path("latest", |_| true)?;
//     let ls_opts = LsOptions::default();
//     let ls = repo.ls(&node, &ls_opts)?;
//
//     let restore_dir = tempdir()?;
//     let dest = LocalDestination::new(restore_dir.path());
//     let restore_opts = RestoreOptions::default();
//     let plan = repo.prepare_restore(&restore_opts, ls.clone(), &dest, false)?;
//     repo.restore(plan, &restore_opts, ls, &dest)?;
//
//     Ok(())
// }

#[rstest]
#[cfg(not(windows))]
fn test_restore_preserves_hardlinks(
    tar_gz_testdata: Result<TestSource>,
    set_up_repo: Result<RepoOpen>,
) -> Result<()> {
    let (source, repo) = (tar_gz_testdata?, set_up_repo?.to_indexed_ids()?);

    let opts = BackupOptions::default().as_path(PathBuf::from_str("test")?);
    let _snapshot = repo.backup(&opts, &source.path_list(), SnapshotFile::default())?;

    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    let ls_opts = LsOptions::default();
    let ls = repo.ls(&node, &ls_opts)?;

    let restore_dir = tempdir()?;
    let dest = LocalDestination::new(restore_dir.path())?;
    let restore_opts = RestoreOptions::default();
    let plan = repo.prepare_restore(&restore_opts, ls.clone(), &dest, false)?;
    repo.restore(plan, &restore_opts, ls, &dest)?;

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
