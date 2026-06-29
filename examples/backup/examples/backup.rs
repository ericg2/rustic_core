//! `backup` example
use rustic_backend::local::{LocalRepo, LocalSource};
use rustic_backend::{BackendBuilder, BackendOptions};
use rustic_core::{
    BackupOptions, CancelToken, Credentials, PathList, Repository, RepositoryOptions,
    SnapshotOptions,
};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let backends = BackendOptions::default()
        .with_repo(&LocalRepo::new("/tmp/repo"))
        .with_repo_hot(&LocalRepo::new("/tmp/repo2"))
        .to_backends()?;

    // Open repository
    let repo_opts = RepositoryOptions::default();
    let credentials = Credentials::password("test");
    let repo = Repository::new(&repo_opts, &backends)?
        .open(&credentials)?
        .to_indexed_ids()?;

    let paths = PathList::from_string("/")?;
    let backup_opts = BackupOptions::default();
    let source = LocalSource::new(paths);
    let snap = SnapshotOptions::default()
        .add_tags("tag1,tag2")?
        .to_snapshot()?;

    // Create snapshot
    let snap = repo.backup(&backup_opts, &source, snap, CancelToken::new())?;

    println!("successfully created snapshot:\n{snap:#?}");
    Ok(())
}
