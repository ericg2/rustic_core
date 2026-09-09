//! `backup` example

use rustic_backend::local::{LocalConfig, LocalSource};
use rustic_backend::opendal::{OpenDALConfig, OpenDALSource};
use rustic_backend::{BackendBuilder, BackendOptions};
use rustic_core::{
    BackupOptions, CancelToken, ConfigOptions, Credentials, KeyOptions, PathList, RepoFileInfo,
    Repository, RepositoryBackends, RepositoryOptions, SnapshotOptions,
};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::collections::HashMap;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let mut opts = HashMap::new();
    opts.insert(
        "root".to_string(),
        "C:\\Users\\Eric\\Documents\\test-repo\\".to_string(),
    );
    let c = OpenDALConfig::default()
        .scheme("fs".to_string())
        .options(opts);
    let backends = BackendOptions::default().with_repo(&c).to_backends()?;

    // Open repository
    let repo_opts = RepositoryOptions::default();
    let credentials = Credentials::password("test");
    let repo = Repository::new(&repo_opts, &backends)?
        .init(
            &credentials,
            &KeyOptions::default(),
            &ConfigOptions::default(),
        )?
        .to_indexed_ids()?;

    let source = LocalSource::new("C:\\Users\\Eric\\Downloads\\Office");
    let snap = SnapshotOptions::default()
        .add_tags("tag1,tag2")?
        .to_snapshot()?;

    // Create snapshot
    let snap = repo.backup(snap).add_source(&source).run()?;

    println!("successfully created snapshot:\n{snap:#?}");
    Ok(())
}
