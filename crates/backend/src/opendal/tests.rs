#[cfg(test)]
mod tests {
    use opendal_ext::config::OpenDALConfig;
    use opendal_ext::Throttle;
    use std::collections::BTreeMap;
    use crate::choose::BackendBuilder;
    use std::str::FromStr;

    use crate::BackendOptions;
    use anyhow::Result;
    use rstest::rstest;
    use rustic_core::{FileType, Id};
    use serde::Deserialize;
    use std::{fs, path::PathBuf};
    use crate::opendal::OpenDALRepo;
    use rustic_core::repofile::SnapshotFile;
    use opendal_ext::Operator;
    use crate::opendal::vfs::*;
    use crate::local::*;
    use tempfile::tempdir;
    use rustic_core::{BackupOptions, CancelToken, ConfigOptions, Credentials, KeyOptions, PathList, Repository, RepositoryOptions};
    // #[rstest]
    // #[case("10kB,10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    // #[case("10 kB,10  MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    // #[case("10kB, 10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    // #[case(" 10kB,   10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    // #[case("10kiB,10MiB", Throttle{bandwidth:10_240, burst:10_485_760})]
    // fn correct_throttle(#[case] input: &str, #[case] expected: Throttle) {
    //     assert_eq!(Throttle::from_str(input).unwrap(), expected);
    // }

    #[rstest]
    #[case("")]
    #[case("10kiB")]
    #[case("no_number,10MiB")]
    #[case("10kB;10MB")]
    fn invalid_throttle(#[case] input: &str) {
        assert!(Throttle::from_str(input).is_err());
    }

    #[test]
    fn force_link_b2() {
        // This does nothing functionally — it just forces the compiler/linker
        // to keep a real reference to opendal's B2 builder type, instead of
        // letting it get stripped as dead code since nothing else touches it.
        let _ = opendal_ext::services::B2::default();
    }

    #[rstest]
    fn new_opendal_backend(
        #[files("tests/fixtures/opendal/*.toml")] test_case: PathBuf,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct TestCase {
            path: String,
            options: BTreeMap<String, String>,
        }

        let test: TestCase = toml::from_str(&fs::read_to_string(test_case)?)?;
        let repo = OpenDALConfig::from_iter(test.path, test.options);
        let be = OpenDALRepo::from_config(repo.clone());
        let _ = BackendOptions::default().with_repo(&be).to_backends()?;

        // Make sure the repository can be serialized and de-serialized as well...
        let s_repo = serde_json::to_string(&repo)?;
        assert!(!s_repo.is_empty());
        let d_repo = serde_json::from_str::<OpenDALConfig>(&s_repo)?;
        assert_eq!(repo, d_repo);

        Ok(())
    }

    /// Test `warmup_path` includes root prefix when root is configured
    #[rstest]
    #[case("s3_aws", "path/to/repo/data/")] // root = "/path/to/repo"
    #[case("s3_idrive", "data/")] // root = "/"
    fn test_warmup_path_respects_root(
        #[case] fixture: &str,
        #[case] expected_prefix: &str,
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct TestCase {
            path: String,
            options: BTreeMap<String, String>,
        }

        let fixture_path = PathBuf::from(format!("tests/fixtures/opendal/{fixture}.toml"));
        let test: TestCase = toml::from_str(&fs::read_to_string(fixture_path)?)?;
        let backend = OpenDALConfig::from_iter(test.path, test.options);
        let backend = OpenDALRepo::from_config(backend);
        let be = BackendOptions::default()
            .with_repo(&backend)
            .to_backends()?;

        let id: Id = "03dc1178e4e54f69beaf35dd9d4256a5a600e9fa3452b9db80bd649938923e67".parse()?;
        let path = be.repository().warmup_path(FileType::Pack, &id);

        assert!(
            path.starts_with(expected_prefix),
            "warmup_path should start with '{expected_prefix}', got: {path}"
        );
        // Verify no double slashes
        assert!(
            !path.contains("//"),
            "warmup_path should not contain double slashes: {path}"
        );

        Ok(())
    }


    /// Strips the leading separator so `/tmp/foo` becomes `tmp/foo`,
    /// matching rustic's file-structure layout inside the VFS.
    fn vfs_path(host_path: &std::path::Path) -> String {
        host_path
            .to_string_lossy()
            .trim_start_matches('/')
            .replace('\\', "/")
            .replace(':', "")
    }

    #[tokio::test]
    async fn backup_and_read_through_vfs() {
        // ── 1. Repository ────────────────────────────────────────────────────
        let repo_dir = tempdir().expect("repo tempdir");
        let local = LocalRepo::new(repo_dir);

        let backends = BackendOptions::default()
            .with_repo(&local)
            .to_backends()
            .expect("backends");

        let repo = Repository::new(&RepositoryOptions::default(), &backends)
            .unwrap()
            .init(
                &Credentials::Password("testing123456!".into()),
                &KeyOptions::default(),
                &ConfigOptions::default(),
            )
            .unwrap()
            .to_indexed_ids() // need IDs to run a backup
            .unwrap();

        // ── 2. Source data ───────────────────────────────────────────────────
        let src_dir = tempdir().expect("source tempdir");
        let src_file = src_dir.path().join("hello.txt");
        fs::write(&src_file, b"hello from backup").expect("write source file");

        // ── 3. Backup ────────────────────────────────────────────────────────
        let paths = PathList::from_iter([src_dir.path()]);
        let mut file = SnapshotFile::default();
        file.hostname = "testvm".into();
        file.label = "test".into();
        file = repo.backup(&BackupOptions::default(), &LocalSource::new(paths), file, CancelToken::new())
            .expect("backup");

        // ── 4. VFS operator ──────────────────────────────────────────────────
        let opts = RusticVfsConfig {
            backend: BackendOptions::default().with_repo(&local),
            credentials: Some(Credentials::Password("testing123456!".into())),
            ..Default::default()
        };

        let op = Operator::from_config(opts).expect("VFS init").finish();

        // ── 5. Assert snapshots/latest exists ────────────────────────────────
        let root = format!("/[{}]/[{}]", &file.hostname, &file.label);
        let latest_meta = op
            .stat(&format!("{}/latest", &root))
            .await
            .expect("/snapshots/latest should exist");

        assert!(latest_meta.is_dir(), "snapshots/latest must be a directory");

        // ── 6. Assert the backed-up file is reachable ────────────────────────
        // Rustic stores /tmp/<hash>/hello.txt as  tmp/<hash>/hello.txt
        let relative = format!("{}/latest/{}/hello.txt", &root, vfs_path(src_dir.path()));

        let file_meta = op
            .stat(&relative)
            .await
            .unwrap_or_else(|_| panic!("expected file at VFS path: {relative}"));
        assert!(file_meta.is_file(), "entry should be a regular file");

        // ── 7. Read and verify content ───────────────────────────────────────
        let bytes = op
            .read(&relative)
            .await
            .unwrap_or_else(|_| panic!("failed to read VFS path: {relative}"));

        assert_eq!(bytes.to_bytes(), "hello from backup".as_bytes());
    }
}
