//! `backup` example

use std::collections::HashMap;
use rustic_backend::BackendOptions;
use rustic_core::{BackupOptions, Credentials, Excludes, FilterOptions, PathList, Repository, RepositoryConfig, RepositoryOptions, RusticResult, SnapshotOptions, WriteBackend};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use rustic_backend::local::{LocalRepo, LocalSaveOptions, LocalSource};
use rustic_backend::opendal::config::B2Config;
use rustic_backend::opendal::{OpenDALRepo, OpenDALSource};

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let backends = BackendOptions::default()
        .with_repo(LocalRepo::new("/tmp/repo"))
        .with_repo_hot(LocalRepo::new("/tmp/repo2"))
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
    let snap = repo.backup(&backup_opts, &source, snap)?;

    println!("successfully created snapshot:\n{snap:#?}");
    Ok(())
}