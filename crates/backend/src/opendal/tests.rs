#[cfg(test)]
mod tests {
    use crate::opendal::Throttle;
    use crate::opendal::{OpenDALConfig, OpenDALSource};
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use crate::BackendOptions;
    use anyhow::Result;
    use rstest::rstest;
    use rustic_core::{FileType, Id};
    use serde::Deserialize;
    use std::{fs, path::PathBuf};

    #[rstest]
    #[case("10kB,10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    #[case("10 kB,10  MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    #[case("10kB, 10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    #[case(" 10kB,   10MB", Throttle{bandwidth:10_000, burst:10_000_000})]
    #[case("10kiB,10MiB", Throttle{bandwidth:10_240, burst:10_485_760})]
    fn correct_throttle(#[case] input: &str, #[case] expected: Throttle) {
        assert_eq!(Throttle::from_str(input).unwrap(), expected);
    }

    #[rstest]
    #[case("")]
    #[case("10kiB")]
    #[case("no_number,10MiB")]
    #[case("10kB;10MB")]
    fn invalid_throttle(#[case] input: &str) {
        assert!(Throttle::from_str(input).is_err());
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
        let _ = BackendOptions::default().with_repo(&repo).to_backends()?;

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
}
