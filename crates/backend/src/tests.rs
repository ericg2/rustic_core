//! Integration tests for `ListAdapter`, exercised against `LocalSource`
//! (a real filesystem backend) so we cover actual `ReadSource` behavior
//! rather than a mock.
//!
//! Backend construction is behind the `TestBackend` trait so the same test
//! bodies can be run against another `ReadSource` + `WriteSource` impl
//! (e.g. an OpenDAL-backed source) by adding a second impl below and
//! swapping `LocalSource` for it in the helpers — no test logic changes.

#[cfg(test)]
mod test {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use crate::local::LocalSource;
    use rustic_core::{Excludes, FilterOptions, ListAdapter, ListOptions, ReadSource, WriteSource};

    /// Constructs a backend rooted at `fs_root` (an absolute filesystem
    /// path). Implement this for any other `ReadSource + WriteSource` to
    /// run the whole suite against it — nothing else in this file needs
    /// to change.
    trait TestBackend: ReadSource + WriteSource + Sized {
        fn open(fs_root: &Path) -> Self;
    }

    impl TestBackend for LocalSource {
        fn open(fs_root: &Path) -> Self {
            LocalSource::new(fs_root)
        }
    }

    /// Swap this alias to point at a different `TestBackend` impl to run
    /// the suite against another backend.
    type Backend = LocalSource;

    /// Collects every yielded path (root-relative, leading `/`, as
    /// `ListAdapter` yields them against a backend rooted at `fs_root`),
    /// panicking on any error so test failures show the actual io::Error
    /// instead of a downstream assertion mismatch.
    fn walk(be: &Backend, list_root: &str, opts: ListOptions) -> Vec<PathBuf> {
        let adapter = ListAdapter::with_options(be, list_root, opts).expect("construct ListAdapter");

        adapter
            .map(|res| res.expect("walk should not error"))
            .map(|file| file.path().to_path_buf())
            .collect()
    }

    /// Convenience wrapper for the common case: backend rooted at `/`
    /// (i.e. the whole temp dir), walked from `/`.
    fn walk_all(be: &Backend, opts: ListOptions) -> Vec<PathBuf> {
        walk(be, "/", opts)
    }

    fn touch(fs_root: &Path, rel: &str, contents: &[u8]) {
        let path = fs_root.join(rel.trim_start_matches('/'));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn mkdir(fs_root: &Path, rel: &str) {
        fs::create_dir_all(fs_root.join(rel.trim_start_matches('/'))).unwrap();
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    fn sorted(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
        v.sort();
        v
    }

    // ── Basic recursive / non-recursive listing ─────────────────────────────

    #[test]
    fn recursive_listing_finds_all_nested_files_and_dirs() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/a.txt", b"1");
        touch(fs_root, "/sub/b.txt", b"2");
        touch(fs_root, "/sub/deeper/c.txt", b"3");

        let be = Backend::open(fs_root);
        let got = sorted(walk_all(&be, ListOptions::default()));

        let expected = sorted(paths(&[
            "/a.txt",
            "/sub",
            "/sub/b.txt",
            "/sub/deeper",
            "/sub/deeper/c.txt",
        ]));

        assert_eq!(got, expected);
    }

    #[test]
    fn non_recursive_listing_only_returns_immediate_children() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/a.txt", b"1");
        touch(fs_root, "/sub/b.txt", b"2");

        let be = Backend::open(fs_root);
        let opts = ListOptions {
            no_recursive: true,
            ..Default::default()
        };
        let got = sorted(walk_all(&be, opts));

        let expected = sorted(paths(&["/a.txt", "/sub"]));

        assert_eq!(got, expected);
    }

    #[test]
    fn empty_directory_yields_no_entries() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        let be = Backend::open(fs_root);
        let got = walk_all(&be, ListOptions::default());

        assert!(got.is_empty());
    }

    // ── The self-entry / abcd-in-abcd regression ────────────────────────────
    // This is the specific bug class we were just chasing: a backend that
    // (incorrectly) surfaces the queried directory as an entry of its own
    // listing must not cause the adapter to fabricate `dir/dir` and recurse.

    #[test]
    fn walking_a_directory_never_yields_itself_as_a_child() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/abcd/file.txt", b"hi");

        let be = Backend::open(fs_root);
        let got = walk_all(&be, ListOptions::default());

        // Regardless of backend quirks, no entry should ever equal its own
        // parent directory (e.g. "abcd" should never itself contain another
        // literal path component "abcd" nested directly under it, coming
        // from a self-referential listing rather than real fs content).
        assert!(
            !got.contains(&PathBuf::from("/abcd/abcd")),
            "found fabricated self-nested path abcd/abcd — self-entry bug regressed: {got:?}"
        );

        let expected = sorted(paths(&["/abcd", "/abcd/file.txt"]));
        assert_eq!(sorted(got), expected);
    }

    #[test]
    fn deeply_nested_same_named_directories_are_not_confused_with_self_entries() {
        // Regression guard specifically for the `child_path == dir` filter:
        // make sure a *legitimately* nested directory sharing its parent's
        // name (e.g. abcd/abcd/file.txt, a real nested dir literally named
        // the same as its parent) is NOT incorrectly filtered out.
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/abcd/abcd/file.txt", b"real nested content");

        let be = Backend::open(fs_root);
        let got = sorted(walk_all(&be, ListOptions::default()));

        let expected = sorted(paths(&["/abcd", "/abcd/abcd", "/abcd/abcd/file.txt"]));

        assert_eq!(
            got, expected,
            "a real directory literally named the same as its parent should still be walked"
        );
    }

    // ── Multi-root ───────────────────────────────────────────────────────────

    #[test]
    fn multi_root_walks_each_root_independently() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/root_a/a.txt", b"1");
        touch(fs_root, "/root_b/b.txt", b"2");

        let be = Backend::open(fs_root);
        // Roots must be paths relative to the backend's own root ("/"),
        // not absolute filesystem paths — the backend was already rooted
        // at `fs_root` via `Backend::open`.
        let adapter = ListAdapter::new_multi(&be, ["/root_a", "/root_b"])
            .expect("construct multi-root adapter");

        let got: Vec<PathBuf> = adapter
            .map(|res| res.expect("walk should not error"))
            .map(|file| file.path().to_path_buf())
            .collect();

        let expected = sorted(paths(&["/root_a/a.txt", "/root_b/b.txt"]));

        assert_eq!(sorted(got), expected);
    }

    #[test]
    fn overlapping_roots_do_not_duplicate_entries() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/sub/a.txt", b"1");

        let be = Backend::open(fs_root);
        // "/sub" is nested under "/"; passing both as roots should not
        // cause "/sub"'s contents to be yielded twice.
        let adapter =
            ListAdapter::new_multi(&be, ["/", "/sub"]).expect("construct overlapping-root adapter");

        let got: Vec<PathBuf> = adapter
            .map(|res| res.expect("walk should not error"))
            .map(|file| file.path().to_path_buf())
            .collect();

        let count_a_txt = got.iter().filter(|p| *p == &PathBuf::from("/sub/a.txt")).count();
        assert_eq!(
            count_a_txt, 1,
            "a.txt should be yielded exactly once despite overlapping roots, got: {got:?}"
        );
    }

    // ── exclude_if_present ────────────────────────────────────────────────────

    #[test]
    fn exclude_if_present_skips_directory_contents_when_marker_file_exists() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/keep/file.txt", b"kept");
        touch(fs_root, "/skip/file.txt", b"skipped");
        touch(fs_root, "/skip/.nobackup", b"");

        let be = Backend::open(fs_root);
        let opts = ListOptions {
            filters: Some(FilterOptions {
                exclude_if_present: vec![".nobackup".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = walk_all(&be, opts);

        // "/skip" itself is still listed as a directory entry (it's a
        // child of root), but its *contents* should be entirely absent
        // because the marker file lives inside it.
        assert!(got.contains(&PathBuf::from("/skip")));
        assert!(!got.contains(&PathBuf::from("/skip/file.txt")));
        assert!(!got.contains(&PathBuf::from("/skip/.nobackup")));

        assert!(got.contains(&PathBuf::from("/keep")));
        assert!(got.contains(&PathBuf::from("/keep/file.txt")));
    }

    // ── exclude_larger_than ───────────────────────────────────────────────────

    #[test]
    fn exclude_larger_than_filters_big_files_but_keeps_directories() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/small.txt", &[0u8; 10]);
        touch(fs_root, "/big.txt", &[0u8; 10_000]);
        touch(fs_root, "/sub/small2.txt", &[0u8; 5]);

        let be = Backend::open(fs_root);
        let opts = ListOptions {
            filters: Some(FilterOptions {
                exclude_larger_than: Some("1KiB".parse().expect("valid ByteSize")),
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = walk_all(&be, opts);

        assert!(got.contains(&PathBuf::from("/small.txt")));
        assert!(!got.contains(&PathBuf::from("/big.txt")));
        // Directories are never subject to the size filter, even if the
        // filesystem reports a nonzero "size" for them.
        assert!(got.contains(&PathBuf::from("/sub")));
        assert!(got.contains(&PathBuf::from("/sub/small2.txt")));
    }

    // ── glob excludes ──────────────────────────────────────────────────────────

    #[test]
    fn glob_exclude_filters_matching_files() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/keep.txt", b"1");
        touch(fs_root, "/skip.log", b"2");
        touch(fs_root, "/sub/skip.log", b"3");

        let be = Backend::open(fs_root);
        let opts = ListOptions {
            excludes: Some(Excludes {
                globs: vec!["!*.log".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = walk_all(&be, opts);

        assert!(got.contains(&PathBuf::from("/keep.txt")));
        assert!(!got.contains(&PathBuf::from("/skip.log")));
        assert!(!got.contains(&PathBuf::from("/sub/skip.log")));
        // The containing directory itself is unaffected by a glob that
        // only matches the file inside it.
        assert!(got.contains(&PathBuf::from("/sub")));
    }

    // ── symlink loop guard ────────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn symlink_cycle_does_not_hang_or_overflow() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        mkdir(fs_root, "/real");
        touch(fs_root, "/real/file.txt", b"1");
        // Cycle: real/loop -> real (points back at its own ancestor).
        symlink(fs_root.join("real"), fs_root.join("real/loop")).unwrap();

        let be = Backend::open(fs_root);
        let adapter =
            ListAdapter::with_options(&be, "/", ListOptions::default()).expect("construct adapter");

        // Just assert it terminates without hitting MAX_DEPTH or hanging.
        // Whether symlinked dirs are followed at all is backend-dependent;
        // the important invariant is termination + no error.
        let results: Vec<_> = adapter.collect();
        for r in &results {
            assert!(r.is_ok(), "walk should not error on a symlink cycle: {r:?}");
        }
    }

    // ── error propagation ──────────────────────────────────────────────────────

    #[test]
    fn nonexistent_root_with_one_file_system_returns_not_found_error() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        let be = Backend::open(fs_root);
        let opts = ListOptions {
            filters: Some(FilterOptions {
                one_file_system: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = ListAdapter::with_options(&be, "/does-not-exist", opts)
            .err()
            .expect("constructing over a missing root should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}