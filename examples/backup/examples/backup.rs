//! `backup` example
use rustic_backend::local::{LocalConfig, LocalSource};
use rustic_backend::{BackendBuilder, BackendOptions};
use rustic_core::{BackupOptions, CancelToken, ConfigOptions, Credentials, KeyOptions, PathList, RepoFileInfo, Repository, RepositoryBackends, RepositoryOptions, SnapshotOptions};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::error::Error;
use rustic_backend::opendal::OpenDALSource;

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let backends = BackendOptions::default()
        .with_repo(&LocalConfig::new("C:\\Users\\Eric\\test-repo\\"))
        .to_backends()?;
    
    // Open repository
    let repo_opts = RepositoryOptions::default();
    let credentials = Credentials::password("test");
    let repo = Repository::new(&repo_opts, &backends)?
        .init(&credentials, &KeyOptions::default(), &ConfigOptions::default())?
        .to_indexed_ids()?;

    let backup_opts = BackupOptions::default();
    let source = LocalSource::new("C:\\Users\\Eric\\Downloads\\Office");
    let snap = SnapshotOptions::default()
        .add_tags("tag1,tag2")?
        .to_snapshot()?;

    // Create snapshot
    let snap = repo.backup(&backup_opts, &source, snap, CancelToken::new())?;
    
    println!("successfully created snapshot:\n{snap:#?}");
    Ok(())
}
