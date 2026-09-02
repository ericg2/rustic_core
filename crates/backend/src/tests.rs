//! Integration tests for `ListAdapter`, exercised against `LocalSource`
//! (a real filesystem backend) so we cover actual `ReadSource` behavior
//! rather than a mock.

#[cfg(test)]
mod test {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use crate::local::LocalSource;
    use rustic_core::{Excludes, FilterOptions, ListAdapter, ListOptions};

    /// Collects every yielded path (relative to `root`), panicking on any
    /// error so test failures show the actual io::Error instead of a
    /// downstream assertion mismatch.
    fn walk_relative(be: &LocalSource, root: &Path, opts: ListOptions) -> Vec<PathBuf> {
        let adapter = ListAdapter::with_options(be, root, opts).expect("construct ListAdapter");

        adapter
            .map(|res| res.expect("walk should not error"))
            .map(|file| {
                file.path()
                    .strip_prefix(root)
                    .expect("yielded path should be under root")
                    .to_path_buf()
            })
            .collect()
    }

    fn as_set(paths: Vec<PathBuf>) -> BTreeSet<PathBuf> {
        paths.into_iter().collect()
    }

    fn touch(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    // ── Basic recursive / non-recursive listing ─────────────────────────────

    #[test]
    fn recursive_listing_finds_all_nested_files_and_dirs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        touch(&root.join("a.txt"), b"1");
        touch(&root.join("sub/b.txt"), b"2");
        touch(&root.join("sub/deeper/c.txt"), b"3");

        let be = LocalSource::new(root);
        let got = as_set(walk_relative(&be, root, ListOptions::default()));

        let expected: BTreeSet<PathBuf> = [
            "a.txt",
            "sub",
            "sub/b.txt",
            "sub/deeper",
            "sub/deeper/c.txt",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();

        assert_eq!(got, expected);
    }

    #[test]
    fn non_recursive_listing_only_returns_immediate_children() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        touch(&root.join("a.txt"), b"1");
        touch(&root.join("sub/b.txt"), b"2");

        let be = LocalSource::new(root);
        let opts = ListOptions {
            no_recursive: true,
            ..Default::default()
        };
        let got = as_set(walk_relative(&be, root, opts));

        let expected: BTreeSet<PathBuf> = ["a.txt", "sub"].into_iter().map(PathBuf::from).collect();

        assert_eq!(got, expected);
    }

    #[test]
    fn empty_directory_yields_no_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let be = LocalSource::new(root);
        let got = walk_relative(&be, root, ListOptions::default());

        assert!(got.is_empty());
    }

    // ── The self-entry / abcd-in-abcd regression ────────────────────────────
    // This is the specific bug class we were just chasing: a backend that
    // (incorrectly) surfaces the queried directory as an entry of its own
    // listing must not cause the adapter to fabricate `dir/dir` and recurse.

    #[test]
    fn walking_a_directory_never_yields_itself_as_a_child() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let nested = root.join("abcd");

        touch(&nested.join("file.txt"), b"hi");

        let be = LocalSource::new(root);
        let got = walk_relative(&be, root, ListOptions::default());

        // Regardless of backend quirks, no entry should ever equal its own
        // parent directory (e.g. "abcd" should never itself contain another
        // literal path component "abcd" nested directly under it, coming from
        // a self-referential listing rather than real fs content).
        assert!(
            !got.contains(&PathBuf::from("abcd/abcd")),
            "found fabricated self-nested path abcd/abcd — self-entry bug regressed: {got:?}"
        );

        let expected: BTreeSet<PathBuf> = ["abcd", "abcd/file.txt"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(as_set(got), expected);
    }

    #[test]
    fn deeply_nested_same_named_directories_are_not_confused_with_self_entries() {
        // Regression guard specifically for the `child_path == dir` filter:
        // make sure a *legitimately* nested directory sharing its parent's
        // name (e.g. abcd/abcd/file.txt, a real nested dir literally named
        // the same as its parent) is NOT incorrectly filtered out.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        touch(&root.join("abcd/abcd/file.txt"), b"real nested content");

        let be = LocalSource::new(root);
        let got = as_set(walk_relative(&be, root, ListOptions::default()));

        let expected: BTreeSet<PathBuf> = ["abcd", "abcd/abcd", "abcd/abcd/file.txt"]
            .into_iter()
            .map(PathBuf::from)
            .collect();

        assert_eq!(
            got, expected,
            "a real directory literally named the same as its parent should still be walked"
        );
    }

    // ── Multi-root ───────────────────────────────────────────────────────────

    #[test]
    fn multi_root_walks_each_root_independently() {
        let tmp = TempDir::new().unwrap();
        let root_a = tmp.path().join("root_a");
        let root_b = tmp.path().join("root_b");

        touch(&root_a.join("a.txt"), b"1");
        touch(&root_b.join("b.txt"), b"2");

        let be = LocalSource::new(tmp.path());
        let adapter =
            ListAdapter::new_multi(&be, [&root_a, &root_b]).expect("construct multi-root adapter");

        let got: BTreeSet<PathBuf> = adapter
            .map(|res| res.expect("walk should not error"))
            .map(|file| file.path().to_path_buf())
            .collect();

        let expected: BTreeSet<PathBuf> = [root_a.join("a.txt"), root_b.join("b.txt")]
            .into_iter()
            .collect();

        assert_eq!(got, expected);
    }

    #[test]
    fn overlapping_roots_do_not_duplicate_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let sub = root.join("sub");

        touch(&sub.join("a.txt"), b"1");

        let be = LocalSource::new(root);
        // `sub` is nested under `root`; passing both as roots should not cause
        // `sub`'s contents to be yielded twice.
        let adapter =
            ListAdapter::new_multi(&be, [root, &sub]).expect("construct overlapping-root adapter");

        let got: Vec<PathBuf> = adapter
            .map(|res| res.expect("walk should not error"))
            .map(|file| file.path().to_path_buf())
            .collect();

        let count_a_txt = got.iter().filter(|p| *p == &sub.join("a.txt")).count();
        assert_eq!(
            count_a_txt, 1,
            "a.txt should be yielded exactly once despite overlapping roots, got: {got:?}"
        );
    }

    // ── exclude_if_present ────────────────────────────────────────────────────

    #[test]
    fn exclude_if_present_skips_directory_contents_when_marker_file_exists() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        touch(&root.join("keep/file.txt"), b"kept");
        touch(&root.join("skip/file.txt"), b"skipped");
        touch(&root.join("skip/.nobackup"), b"");

        let be = LocalSource::new(root);
        let opts = ListOptions {
            filters: Some(FilterOptions {
                exclude_if_present: vec![".nobackup".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = as_set(walk_relative(&be, root, opts));

        // `skip/` itself is still listed as a directory entry (it's a child of
        // root), but its *contents* should be entirely absent because the
        // marker file lives inside it.
        assert!(got.contains(&PathBuf::from("skip")));
        assert!(!got.contains(&PathBuf::from("skip/file.txt")));
        assert!(!got.contains(&PathBuf::from("skip/.nobackup")));

        assert!(got.contains(&PathBuf::from("keep")));
        assert!(got.contains(&PathBuf::from("keep/file.txt")));
    }

    // ── exclude_larger_than ───────────────────────────────────────────────────

    #[test]
    fn exclude_larger_than_filters_big_files_but_keeps_directories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        touch(&root.join("small.txt"), &[0u8; 10]);
        touch(&root.join("big.txt"), &[0u8; 10_000]);
        touch(&root.join("sub/small2.txt"), &[0u8; 5]);

        let be = LocalSource::new(root);
        let opts = ListOptions {
            filters: Some(FilterOptions {
                exclude_larger_than: Some("1KiB".parse().expect("valid ByteSize")),
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = as_set(walk_relative(&be, root, opts));

        assert!(got.contains(&PathBuf::from("small.txt")));
        assert!(!got.contains(&PathBuf::from("big.txt")));
        // Directories are never subject to the size filter, even if the
        // filesystem reports a nonzero "size" for them.
        assert!(got.contains(&PathBuf::from("sub")));
        assert!(got.contains(&PathBuf::from("sub/small2.txt")));
    }

    // ── glob excludes ──────────────────────────────────────────────────────────

    #[test]
    fn glob_exclude_filters_matching_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        touch(&root.join("keep.txt"), b"1");
        touch(&root.join("skip.log"), b"2");
        touch(&root.join("sub/skip.log"), b"3");

        let be = LocalSource::new(root);
        let opts = ListOptions {
            excludes: Some(Excludes {
                globs: vec!["*.log".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let got = as_set(walk_relative(&be, root, opts));

        assert!(got.contains(&PathBuf::from("keep.txt")));
        assert!(!got.contains(&PathBuf::from("skip.log")));
        assert!(!got.contains(&PathBuf::from("sub/skip.log")));
        // The containing directory itself is unaffected by a glob that only
        // matches the file inside it.
        assert!(got.contains(&PathBuf::from("sub")));
    }

    // ── symlink loop guard ────────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn symlink_cycle_does_not_hang_or_overflow() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("real")).unwrap();
        touch(&root.join("real/file.txt"), b"1");
        // Cycle: real/loop -> .. -> real (points back at its own ancestor).
        symlink(root.join("real"), root.join("real/loop")).unwrap();

        let be = LocalSource::new(root);
        let adapter = ListAdapter::with_options(&be, root, ListOptions::default())
            .expect("construct adapter");

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
        let missing = tmp.path().join("does-not-exist");

        let be = LocalSource::new(tmp.path());
        let opts = ListOptions {
            filters: Some(FilterOptions {
                one_file_system: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = ListAdapter::with_options(&be, &missing, opts)
            .err()
            .expect("constructing over a missing root should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
