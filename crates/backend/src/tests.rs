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
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use crate::local::LocalSource;
    use rustic_core::{Excludes, FilterOptions, ListBuilder, ListOptions, ReadSource, WriteSource};
    #[cfg(feature = "opendal")]
    use crate::opendal::{OpenDALConfig, OpenDALSource};

    /// Constructs a backend rooted at `fs_root` (an absolute filesystem
    /// path). Implement this for any other `ReadSource + WriteSource` to
    /// run the whole suite against it — nothing else in this file needs
    /// to change.
    trait TestBackend: ReadSource + WriteSource + Sized {
        fn open(fs_root: &Path) -> Self;
    }

    impl TestBackend for LocalSource {
        fn open(fs_root: &Path) -> Self {
            Self::new(fs_root)
        }
    }

    #[cfg(feature = "opendal")]
    impl TestBackend for OpenDALSource {
        fn open(fs_root: &Path) -> Self {
            let mut opts = HashMap::new();
            let _ = opts.insert("root".to_string(), fs_root.to_str().unwrap().to_string());

            let cfg = OpenDALConfig::default().options(opts).scheme("fs".to_string());
            Self::from_config(&cfg).unwrap()
        }
    }

    /// Swap this alias to point at a different `TestBackend` impl to run
    /// the suite against another backend. The `OpenDAL` implementation above
    /// is compiled behind the `opendal` feature, so the always-available
    /// local backend is the default target.
    type Backend = LocalSource;

    /// Collects every yielded path (root-relative, leading `/`, as
    /// `ListBuilder` yields them against a backend rooted at `fs_root`),
    /// panicking on any error so test failures show the actual `io::Error`
    /// instead of a downstream assertion mismatch.
    fn walk<R: ReadSource>(be: &R, list_root: &str, opts: ListOptions) -> Vec<PathBuf> {
        let adapter = ListBuilder::new(be)
            .with_root(list_root)
            .with_options(opts)
            .build()
            .expect("construct ListAdapter");

        adapter
            .map(|res| res.expect("walk should not error"))
            .map(|file| file.path().to_path_buf())
            .collect()
    }

    /// Convenience wrapper for the common case: backend rooted at `/`
    /// (i.e. the whole temp dir), walked from `/`.
    fn walk_all<R: ReadSource>(be: &R, opts: ListOptions) -> Vec<PathBuf> {
        walk(be, "/", opts)
    }

    fn read_all<R: ReadSource>(be: &R, path: &str) -> Vec<u8> {
        use std::io::Read;

        let mut handle = be
            .open_read(Path::new(path))
            .unwrap_or_else(|err| panic!("open {path} failed: {err}"));
        let mut buf = Vec::new();
        let _ = handle
            .read_to_end(&mut buf)
            .unwrap_or_else(|err| panic!("read {path} failed: {err}"));
        handle
            .close()
            .unwrap_or_else(|err| panic!("close {path} failed: {err}"));
        buf
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

        assert_eq!(got, Vec::<PathBuf>::new());
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
        let adapter = ListBuilder::new(&be)
            .with_roots(["/root_a", "/root_b"])
            .build()
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
        let adapter = ListBuilder::new(&be)
            .with_roots(["/", "/sub"])
            .build()
            .expect("construct overlapping-root adapter");

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
        let adapter = ListBuilder::new(&be)
            .with_root("/")
            .with_options(ListOptions::default())
            .build()
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
        let fs_root = tmp.path();

        let be = Backend::open(fs_root);
        let opts = ListOptions {
            filters: Some(FilterOptions {
                one_file_system: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        let err = ListBuilder::new(&be)
            .with_root("/does-not-exist")
            .with_options(opts)
            .build()
            .err()
            .expect("constructing over a missing root should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // ── Root prefixing (non-"/" root) ────────────────────────────────────────

    #[test]
    fn walking_a_root_below_filesystem_root_prefixes_every_yielded_path() {
        // All existing tests walk from "/", which never exercises whether the
        // adapter actually prepends the root it was given — every path would
        // "coincidentally" look correct even if the prefix were silently
        // dropped. This walks from a nested root instead, so a dropped-prefix
        // regression (e.g. an absolute-looking name winning a `PathBuf::join`)
        // would show up as bare "/xyz.exe" instead of "/restore/xyz.exe".
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/restore/xyz.exe", b"payload");
        touch(fs_root, "/restore/sub/deeper.txt", b"nested");
        // A sibling outside the walked root — must never appear in results,
        // and definitely must never appear stripped-down as "/other.txt"
        // colliding with something the walk itself would produce.
        touch(fs_root, "/other.txt", b"should not appear");

        let be = Backend::open(fs_root);
        let got = sorted(walk(&be, "/restore", ListOptions::default()));

        let expected = sorted(paths(&[
            "/restore/xyz.exe",
            "/restore/sub",
            "/restore/sub/deeper.txt",
        ]));

        assert_eq!(got, expected);

        // Every single yielded path must be rooted under "/restore" — none of
        // them should have come back relative to "/" instead.
        for p in &got {
            assert!(
                p.starts_with("/restore"),
                "path `{}` was not prefixed with the walk root `/restore` — \
             the root prefix was dropped",
                p.display()
            );
        }

        // The specific failure mode reported: reading back a bare, unprefixed
        // name must not spuriously "exist" just because a same-named file
        // happens to live outside the walked root.
        assert!(!got.contains(&PathBuf::from("/xyz.exe")));
    }

    #[test]
    fn walking_a_deeply_nested_root_keeps_full_prefix_at_every_depth() {
        // Same idea, but nested several levels deep, and recursing further,
        // to make sure the prefix survives being carried forward through
        // `PendingDir` across multiple `next()` iterations, not just on the
        // first listing.
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/a/b/c/file1.txt", b"1");
        touch(fs_root, "/a/b/c/d/file2.txt", b"2");

        let be = Backend::open(fs_root);
        let got = sorted(walk(&be, "/a/b/c", ListOptions::default()));

        let expected = sorted(paths(&[
            "/a/b/c/file1.txt",
            "/a/b/c/d",
            "/a/b/c/d/file2.txt",
        ]));

        assert_eq!(got, expected);
        for p in &got {
            assert!(p.starts_with("/a/b/c"), "lost prefix at: {}", p.display());
        }
    }

    // ── LocalSource read/write operations ──────────────────────────────────

    #[test]
    fn local_source_stat_reports_metadata_and_missing_paths() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/dir/file.txt", b"hello");
        let be = LocalSource::new(fs_root);

        let meta = be
            .stat(Path::new("/dir/file.txt"))
            .expect("stat should succeed")
            .expect("file should exist");
        assert_eq!(meta.size, 5);
        assert!(meta.mtime.is_some());

        let dir_meta = be
            .stat(Path::new("/dir"))
            .expect("stat should succeed")
            .expect("dir should exist");
        assert_eq!(dir_meta.size, 0);

        assert!(
            be.stat(Path::new("/does-not-exist"))
                .expect("stat should distinguish missing paths")
                .is_none()
        );
    }

    #[test]
    fn local_source_open_read_is_relative_to_the_backend_root() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/nested/file.txt", b"hello from the root");
        let be = LocalSource::new(fs_root);

        assert_eq!(read_all(&be, "/nested/file.txt"), b"hello from the root");

        // A root-prefixed path must be resolved below the backend root, not
        // treated as an absolute filesystem path.
        assert!(be.open_read(Path::new("/nested/missing.txt")).is_err());
    }

    #[test]
    fn local_source_write_all_creates_parents_and_truncates() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();
        let be = LocalSource::new(fs_root);

        be.write_all(
            Path::new("/nested/dir/file.txt"),
            b"first version".to_vec().into(),
        )
        .expect("write_all should create missing parents");
        assert_eq!(
            fs::read_to_string(fs_root.join("nested/dir/file.txt")).unwrap(),
            "first version"
        );
        assert_eq!(read_all(&be, "/nested/dir/file.txt"), b"first version");

        // Writing again must replace (not append to) the file.
        be.write_all(Path::new("/nested/dir/file.txt"), b"second".to_vec().into())
            .expect("write_all should replace existing files");
        assert_eq!(read_all(&be, "/nested/dir/file.txt"), b"second");

        // The atomic-write helper must not leave its temporary file behind.
        let entries: Vec<_> = fs::read_dir(fs_root.join("nested/dir"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("file.txt")]);
    }

    #[test]
    fn local_source_write_at_and_set_length_modify_existing_content() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();
        let be = LocalSource::new(fs_root);

        be.write_all(Path::new("/file.bin"), b"0123456789".to_vec().into())
            .unwrap();

        be.write_at(Path::new("/file.bin"), 4, b"xy")
            .expect("write_at should overwrite bytes at the given offset");
        assert_eq!(read_all(&be, "/file.bin"), b"0123xy6789");

        be.set_length(Path::new("/file.bin"), 4)
            .expect("set_length should truncate the file");
        assert_eq!(read_all(&be, "/file.bin"), b"0123");
    }

    #[test]
    fn local_source_hard_link_creates_a_second_name_for_the_same_inode() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();
        let be = LocalSource::new(fs_root);

        be.write_all(Path::new("/original.txt"), b"linked".to_vec().into())
            .unwrap();
        be.hard_link(Path::new("/original.txt"), Path::new("/link.txt"))
            .expect("hard_link should create the link");

        assert_eq!(read_all(&be, "/link.txt"), b"linked");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let original = fs::metadata(fs_root.join("original.txt")).unwrap();
            let link = fs::metadata(fs_root.join("link.txt")).unwrap();
            assert_eq!(original.ino(), link.ino());
            assert_eq!(link.nlink(), 2);
        }
    }

    #[test]
    fn local_source_remove_file_and_remove_dir_delete_below_the_root() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();
        let be = LocalSource::new(fs_root);

        be.write_all(Path::new("/file.txt"), b"x".to_vec().into())
            .unwrap();
        be.remove_file(Path::new("/file.txt")).unwrap();
        assert!(!fs_root.join("file.txt").exists());

        be.create_dir_all(Path::new("/a/b/c")).unwrap();
        be.write_all(Path::new("/a/b/c/file.txt"), b"x".to_vec().into())
            .unwrap();
        be.remove_dir(Path::new("/a")).unwrap();
        assert!(!fs_root.join("a").exists());

        // Removing a missing file must surface an error rather than succeed
        // silently.
        assert!(be.remove_file(Path::new("/missing.txt")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn local_source_readdir_reports_symlinks_instead_of_following_them() {
        use std::os::unix::fs::symlink;

        use rustic_core::NodeType;

        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/target.txt", b"target content");
        symlink("target.txt", fs_root.join("link.txt")).unwrap();

        let be = LocalSource::new(fs_root);
        let nodes: Vec<_> = be
            .readdir(Path::new("/"))
            .expect("readdir should succeed")
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();

        let link = nodes
            .iter()
            .find(|node| node.name() == std::ffi::OsStr::new("link.txt"))
            .expect("symlink should be listed");
        assert!(matches!(link.node_type, NodeType::Symlink { .. }));
        assert_eq!(link.node_type.to_link(), Path::new("target.txt"));

        // `stat` uses symlink metadata as well, so the target is not followed.
        let link_meta = be
            .stat(Path::new("/link.txt"))
            .unwrap()
            .expect("symlink should exist");
        assert_eq!(link_meta.size, "target.txt".len() as u64);
    }

    // ── OpenDAL fs backend ────────────────────────────────────────────────

    #[cfg(feature = "opendal")]
    #[test]
    fn opendal_fs_source_lists_the_same_tree_as_local_source() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();

        touch(fs_root, "/a.txt", b"1");
        touch(fs_root, "/sub/b.txt", b"2");
        touch(fs_root, "/sub/deeper/c.txt", b"3");

        let local = LocalSource::new(fs_root);
        let opendal = <OpenDALSource as TestBackend>::open(fs_root);

        let local_paths = sorted(walk_all(&local, ListOptions::default()));
        let opendal_paths = sorted(walk_all(&opendal, ListOptions::default()));

        assert_eq!(opendal_paths, local_paths);
    }

    #[cfg(feature = "opendal")]
    #[test]
    fn opendal_fs_source_supports_write_read_stat_and_delete() {
        let tmp = TempDir::new().unwrap();
        let fs_root = tmp.path();
        let be = <OpenDALSource as TestBackend>::open(fs_root);

        be.create_dir_all(Path::new("/nested")).unwrap();
        be.write_all(Path::new("/nested/file.txt"), b"opendal".to_vec().into())
            .expect("OpenDAL write_all should succeed");
        assert_eq!(read_all(&be, "/nested/file.txt"), b"opendal");

        let meta = be
            .stat(Path::new("/nested/file.txt"))
            .expect("stat should succeed")
            .expect("file should exist");
        assert_eq!(meta.size, 7);
        assert!(be.stat(Path::new("/nested/missing.txt")).unwrap().is_none());

        be.remove_file(Path::new("/nested/file.txt")).unwrap();
        assert!(!fs_root.join("nested/file.txt").exists());

        be.create_dir_all(Path::new("/nested/deeper")).unwrap();
        be.write_all(
            Path::new("/nested/deeper/file.txt"),
            b"opendal".to_vec().into(),
        )
        .unwrap();
        be.remove_dir(Path::new("/nested")).unwrap();
        assert!(!fs_root.join("nested").exists());
    }

    #[cfg(feature = "opendal")]
    #[test]
    fn opendal_fs_source_reports_unsupported_random_writes() {
        let tmp = TempDir::new().unwrap();
        let be = <OpenDALSource as TestBackend>::open(tmp.path());

        be.write_all(Path::new("/file.txt"), b"data".to_vec().into())
            .unwrap();

        assert!(!be.can_random_write());
        assert!(!be.can_hard_link());
        assert_eq!(
            be.write_at(Path::new("/file.txt"), 0, b"x")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
        assert_eq!(
            be.hard_link(Path::new("/file.txt"), Path::new("/link.txt"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );

        // Truncating to zero is supported; growing is not.
        be.set_length(Path::new("/file.txt"), 0).unwrap();
        assert_eq!(read_all(&be, "/file.txt"), b"");
        assert_eq!(
            be.set_length(Path::new("/file.txt"), 1).unwrap_err().kind(),
            std::io::ErrorKind::Unsupported
        );
    }
}