//! `restore` example
use rustic_backend::BackendOptions;
use rustic_core::{
    Credentials, LsOptions, Repository, RepositoryOptions, RestoreOptions,
};
use simplelog::{Config, LevelFilter, SimpleLogger};
use std::error::Error;
use rustic_backend::local::LocalDestination;

fn main() -> Result<(), Box<dyn Error>> {
    // Display info logs
    let _ = SimpleLogger::init(LevelFilter::Info, Config::default());

    // Initialize Backends
    let backends = BackendOptions::default()
        .repository("/tmp/repo")
        .to_backends()?;

    // Open repository
    let repo_opts = RepositoryOptions::default();
    let repo = Repository::new(&repo_opts, &backends)?
        .open(&Credentials::password("test"))?
        .to_indexed()?;

    // use latest snapshot without filtering snapshots
    let node = repo.node_from_snapshot_path("latest", |_| true)?;

    // use list of the snapshot contents using no additional filtering
    let streamer_opts = LsOptions::default();
    let ls = repo.ls(&node, &streamer_opts)?;

    let destination = "./restore/"; // restore to this destination dir
    let dest = LocalDestination::new(destination);
    let opts = RestoreOptions::default();
    let dry_run = false;
    
    // create restore infos. Note: this also already creates needed dirs in the destination
    let restore_infos = repo.prepare_restore(&opts, ls.clone(), &dest, dry_run)?;

    repo.restore(restore_infos, &opts, ls, &dest)?;
    Ok(())
}
