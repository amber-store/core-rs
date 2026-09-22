//! End-to-end tests of the `amber-store` CLI example: a port of Go
//! `cmd/amber-store/e2e_test.go`'s pin..HEAD delta
//! (`TestE2E_RefSetChecksCompleteness`, `TestE2E_RefLifecycle`,
//! `TestE2E_GC`) plus the pre-existing `TestE2E_MissingStoreFlag`, which
//! had no Rust counterpart yet, and `TestE2E_Commit` (Go PR #12). The Go
//! unit tests of `parseIdentity` and `renderCommit` are covered through the
//! CLI in `commit_identity_and_rendering`: the example carries no unit
//! tests. Go drives `newApp()` in-process; here each
//! case spawns the compiled example binary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness (Go: e2e_test.go's runApp / writeFixture).
// ---------------------------------------------------------------------------

/// Locates the example binary next to the test executable
/// (`target/<profile>/examples/amber-store`). `cargo test` builds examples
/// before tests; a bare test-harness invocation may not have, so build it
/// through the toolchain that compiled this test as a fallback.
fn cli_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let mut dir = std::env::current_exe().expect("current_exe");
        dir.pop(); // the test binary's own name
        if dir.ends_with("deps") {
            dir.pop();
        }
        let bin = dir.join("examples").join("amber-store");
        if !bin.exists() {
            let mut build = Command::new(env!("CARGO"));
            build.args(["build", "--example", "amber-store"]);
            if dir.file_name().is_some_and(|n| n == "release") {
                build.arg("--release");
            }
            let status = build.status().expect("spawn cargo build");
            assert!(status.success(), "cargo build --example amber-store failed");
        }
        assert!(bin.exists(), "no example binary at {}", bin.display());
        bin
    })
}

/// Runs the CLI with `args` and returns everything it printed to stdout; a
/// non-zero exit becomes an `Err` carrying stderr (Go: `runApp`).
/// $AMBER_STORE is forced empty, as Go's `TestE2E_MissingStoreFlag` does
/// with `t.Setenv` — every other case passes --store explicitly.
fn run_app<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Result<String, String> {
    let out = Command::new(cli_bin())
        .args(args)
        .env("AMBER_STORE", "")
        .output()
        .expect("spawn amber-store");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "exit {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim_end()
        ))
    }
}

/// Runs the CLI with the shared `--store <dir> --segment-size 4096` prefix
/// (Go: the `seg` slice; 4 KiB segments force sealing on small fixtures).
fn run_seg(store: &Path, rest: &[&str]) -> Result<String, String> {
    let mut args = vec![
        "--store".to_string(),
        store.display().to_string(),
        "--segment-size".to_string(),
        "4096".to_string(),
    ];
    args.extend(rest.iter().map(|s| s.to_string()));
    run_app(&args)
}

/// Builds a small source tree: files, a subdirectory, a symlink (Go:
/// `writeFixture`).
fn write_fixture(dir: &Path) {
    fs::write(dir.join("a.txt"), b"alpha").unwrap();
    let sub = dir.join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("b.txt"), b"beta").unwrap();
    fs::set_permissions(sub.join("b.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
}

// ---------------------------------------------------------------------------
// Ported tests (Go: cmd/amber-store/e2e_test.go).
// ---------------------------------------------------------------------------

// Port of Go TestE2E_MissingStoreFlag.
#[test]
fn missing_store_flag() {
    let key = "00".repeat(32);
    assert!(
        run_app(&["ls", key.as_str()]).is_err(),
        "expected an error without --store / $AMBER_STORE"
    );
}

// Port of Go TestE2E_RefSetChecksCompleteness.
#[test]
fn ref_set_checks_completeness() {
    let store = TempDir::new().unwrap();
    // A syntactically valid key that names nothing in the store.
    let bogus = "00".repeat(32);
    let store_s = store.path().display().to_string();
    assert!(
        run_app(&["--store", &store_s, "ref", "set", "v1", &bogus]).is_err(),
        "ref set to an absent key succeeded"
    );
}

// Port of Go TestE2E_RefLifecycle (timing: two 50 ms sleeps so the pack
// seals cross the 1 ms grace).
#[test]
fn ref_lifecycle() {
    let src = TempDir::new().unwrap();
    write_fixture(src.path());
    let store = TempDir::new().unwrap();
    let src_s = src.path().display().to_string();

    let out = run_seg(store.path(), &["ingest", "--no-progress", &src_s]).unwrap();
    let root = out.trim().to_string();
    if let Err(e) = run_seg(store.path(), &["ref", "set", "v1", &root]) {
        panic!("ref set: {e}");
    }
    // A second name shares the root; removing one keeps the tree live.
    run_seg(store.path(), &["ref", "set", "v2", &root]).unwrap();
    run_seg(store.path(), &["ref", "rm", "v1"]).unwrap();
    thread::sleep(Duration::from_millis(50));
    if let Err(e) = run_seg(
        store.path(),
        &["gc", "run", "--grace", "1ms", "--garbage", "0"],
    ) {
        panic!("gc run while v2 lives: {e}");
    }
    let restore_dir = TempDir::new().unwrap();
    let restore_s = restore_dir.path().display().to_string();
    if let Err(e) = run_seg(store.path(), &["restore", "ref:v2", &restore_s]) {
        panic!("restore after gc while v2 lives: {e}");
    }
    // The last rm makes the tree garbage; the next cycle collects it.
    run_seg(store.path(), &["ref", "rm", "v2"]).unwrap();
    thread::sleep(Duration::from_millis(50));
    if let Err(e) = run_seg(
        store.path(),
        &["gc", "run", "--grace", "1ms", "--garbage", "0"],
    ) {
        panic!("gc run after last rm: {e}");
    }
    let out = match run_seg(store.path(), &["gc", "why", &root]) {
        Ok(out) => out,
        Err(e) => panic!("gc why: {e}"),
    };
    assert!(
        out.contains("unreferenced"),
        "gc why after last rm = {out:?}, want unreferenced"
    );
}

// Port of Go TestE2E_GC (timing: one 50 ms sleep before the forced run).
#[test]
fn gc_end_to_end() {
    let src = TempDir::new().unwrap();
    write_fixture(src.path());
    let store = TempDir::new().unwrap();
    let src_s = src.path().display().to_string();

    let out = match run_seg(
        store.path(),
        &["ingest", "--no-progress", "--ref", "v1", &src_s],
    ) {
        Ok(out) => out,
        Err(e) => panic!("ingest: {e}"),
    };
    let root1 = out.trim().to_string();

    // The tree changes; v1 moves on, orphaning the first tree's unique data.
    fs::write(src.path().join("a.txt"), "fresh content\n".repeat(200)).unwrap();
    let out = match run_seg(
        store.path(),
        &["ingest", "--no-progress", "--ref", "v1", &src_s],
    ) {
        Ok(out) => out,
        Err(e) => panic!("second ingest: {e}"),
    };
    let root2 = out.trim().to_string();
    assert_ne!(root1, root2, "fixture change did not change the root");

    // status runs and mentions the store's packs.
    let out = match run_seg(store.path(), &["gc", "status"]) {
        Ok(out) => out,
        Err(e) => panic!("gc status: {e}"),
    };
    assert!(
        out.contains("live"),
        "gc status output {out:?} missing totals"
    );

    // A forced run with a tiny grace reaps the dead majority. (References
    // written by ingest --ref carry closures since the collector wiring
    // landed; the cycle would also walk any that were missing.)
    thread::sleep(Duration::from_millis(50)); // put seals safely behind a 1ms grace
    let out = match run_seg(
        store.path(),
        &["gc", "run", "--grace", "1ms", "--garbage", "0"],
    ) {
        Ok(out) => out,
        Err(e) => panic!("gc run: {e}"),
    };
    assert!(
        out.contains("reaped"),
        "gc run output {out:?} missing summary"
    );

    // why: the new root is held by v1; the old root by nobody.
    let out = match run_seg(store.path(), &["gc", "why", &root2]) {
        Ok(out) => out,
        Err(e) => panic!("gc why: {e}"),
    };
    assert!(out.contains("v1"), "gc why {out:?} missing v1");
    let out = match run_seg(store.path(), &["gc", "why", &root1]) {
        Ok(out) => out,
        Err(e) => panic!("gc why old: {e}"),
    };
    assert!(
        !out.contains("v1"),
        "gc why on dead root still names v1: {out:?}"
    );

    // The referenced tree is fully intact after the sweep.
    let tar_dir = TempDir::new().unwrap();
    let tar_path = tar_dir.path().join("out.tar").display().to_string();
    if let Err(e) = run_seg(store.path(), &["export", "-o", &tar_path, "ref:v1"]) {
        panic!("export after gc: {e}");
    }
    let restore_dir = TempDir::new().unwrap();
    let restore_s = restore_dir.path().display().to_string();
    if let Err(e) = run_seg(store.path(), &["restore", "ref:v1", &restore_s]) {
        panic!("restore after gc: {e}");
    }
    let got = fs::read_to_string(restore_dir.path().join("a.txt")).unwrap();
    assert!(
        got.starts_with("fresh content"),
        "restored content wrong after gc"
    );
}

// ---------------------------------------------------------------------------
// Commits (Go: TestE2E_Commit, plus commit_test.go's parseIdentity and
// renderCommit cases driven through the CLI).
// ---------------------------------------------------------------------------

#[test]
fn commit_end_to_end() {
    let src = TempDir::new().unwrap();
    write_fixture(src.path());
    let store = TempDir::new().unwrap();
    let src_s = src.path().display().to_string();
    let run = |args: &[&str]| -> String {
        match run_seg(store.path(), args) {
            Ok(out) => out.trim().to_string(),
            Err(e) => panic!("{args:?}: {e}"),
        }
    };
    let create = |extra: &[&str]| -> String {
        let mut args = vec![
            "commit",
            "create",
            "--ref",
            "main",
            "--author",
            "Ann <ann@example.com>",
            "--date",
            "2026-01-02T03:04:05+01:00",
        ];
        args.extend_from_slice(extra);
        run(&args)
    };

    let root1 = run(&["ingest", "--no-progress", &src_s]);
    let c1 = create(&["-m", "first", &root1]);

    fs::write(src.path().join("a.txt"), "alpha, revised").unwrap();
    let root2 = run(&["ingest", "--no-progress", &src_s]);
    let c2 = create(&["--parent", "ref:main", "-m", "second", &root2]);
    assert!(c1 != c2 && c2 != root2, "c1 {c1}, c2 {c2}, root2 {root2}");
    assert_eq!(run(&["ref", "get", "main"]), c2, "ref get main");

    let show = run(&["commit", "show", "ref:main"]);
    for want in [
        format!("commit {c2}"),
        format!("tree {root2}"),
        format!("parent {c1}"),
        "author Ann <ann@example.com> 2026-01-02T03:04:05+01:00".to_string(),
        "    second".to_string(),
    ] {
        assert!(
            show.contains(&want),
            "commit show {show:?} is missing {want:?}"
        );
    }

    // A commit stands in for its tree wherever a directory is expected.
    for (spec, want) in [
        ("ref:main".to_string(), "a.txt"),
        ("ref:main@sub".to_string(), "b.txt"),
        (c1.clone(), "a.txt"),
        (format!("{c1}/sub"), "b.txt"),
    ] {
        let out = run(&["ls", &spec]);
        assert!(out.contains(want), "ls {spec} output {out:?} lacks {want}");
    }

    // History is reachable from the branch, so a forced gc keeps the first
    // commit's tree: it still restores, with the original content.
    thread::sleep(Duration::from_millis(50));
    run(&["gc", "run", "--grace", "1ms", "--garbage", "0"]);
    for (commit_key, want) in [(&c1, "alpha"), (&c2, "alpha, revised")] {
        let dest = TempDir::new().unwrap();
        let dest_s = dest.path().join("restored").display().to_string();
        run(&["restore", commit_key, &dest_s]);
        let got = fs::read_to_string(dest.path().join("restored").join("a.txt")).unwrap();
        assert_eq!(got, want, "restored a.txt of {commit_key}");
    }

    // Rejections: a tree as a parent, a file as the tree, a missing author,
    // showing something that is not a commit.
    let file_spec = format!("{root2}/a.txt");
    for (name, args) in [
        (
            "tree as parent",
            vec![
                "commit", "create", "--author", "Ann", "-m", "x", "--parent", &root1, &root2,
            ],
        ),
        (
            "file as tree",
            vec!["commit", "create", "--author", "Ann", "-m", "x", &file_spec],
        ),
        ("no author", vec!["commit", "create", "-m", "x", &root2]),
        ("show a tree", vec!["commit", "show", &root2]),
    ] {
        assert!(
            run_seg(store.path(), &args).is_err(),
            "{name}: command succeeded"
        );
    }
}

#[test]
fn commit_identity_and_rendering() {
    let src = TempDir::new().unwrap();
    write_fixture(src.path());
    let store = TempDir::new().unwrap();
    let src_s = src.path().display().to_string();
    let tree = run_seg(store.path(), &["ingest", "--no-progress", &src_s])
        .unwrap()
        .trim()
        .to_string();

    // Go's TestRenderCommit layout, byte for byte: headers, a blank line, the
    // message indented by four spaces with trailing newlines dropped; the
    // time rendered in the identity's own zone; an identity without an email
    // prints the bare name. (The "signature N bytes" line is unreachable
    // through the CLI, which cannot create signed commits.)
    let key = run_seg(
        store.path(),
        &[
            "commit",
            "create",
            "--author",
            "  Ann  <ann@example.com>  ",
            "--committer",
            "Bob",
            "--date",
            "2026-01-01T22:04:05-05:00",
            "-m",
            "subject\n\nbody\n",
            &tree,
        ],
    )
    .unwrap()
    .trim()
    .to_string();
    let show = run_seg(store.path(), &["commit", "show", &key]).unwrap();
    assert_eq!(
        show,
        format!(
            "commit {key}\ntree {tree}\n\
             author Ann <ann@example.com> 2026-01-01T22:04:05-05:00\n\
             committer Bob 2026-01-01T22:04:05-05:00\n\
             \n    subject\n    \n    body\n"
        )
    );

    // The same instant given in another zone is a different commit: the
    // offset is part of the record.
    let utc = run_seg(
        store.path(),
        &[
            "commit",
            "create",
            "--author",
            "Ann <ann@example.com>",
            "--committer",
            "Bob",
            "--date",
            "2026-01-02T03:04:05Z",
            "-m",
            "subject\n\nbody\n",
            &tree,
        ],
    )
    .unwrap();
    assert_ne!(utc.trim(), key);
    let show = run_seg(store.path(), &["commit", "show", utc.trim()]).unwrap();
    assert!(
        show.contains("committer Bob 2026-01-02T03:04:05Z\n"),
        "{show:?}"
    );

    // An empty message prints no blank line and no body.
    let bare = run_seg(
        store.path(),
        &[
            "commit",
            "create",
            "--author",
            "A <b> C <c@d>",
            "--date",
            "2026-01-02T03:04:05Z",
            &tree,
        ],
    )
    .unwrap();
    let show = run_seg(store.path(), &["commit", "show", bare.trim()]).unwrap();
    assert!(
        show.ends_with("committer A <b> C <c@d> 2026-01-02T03:04:05Z\n"),
        "{show:?}"
    );

    // parseIdentity's rejections and the other argument errors.
    for (name, args) in [
        (
            "nameless author",
            vec!["commit", "create", "--author", "<ann@example.com>", &tree],
        ),
        (
            "empty author",
            vec!["commit", "create", "--author", "", &tree],
        ),
        (
            "bad date",
            vec![
                "commit",
                "create",
                "--author",
                "Ann",
                "--date",
                "yesterday",
                &tree,
            ],
        ),
        (
            "date without zone",
            vec![
                "commit",
                "create",
                "--author",
                "Ann",
                "--date",
                "2026-01-02T03:04:05",
                &tree,
            ],
        ),
        (
            "absent parent",
            vec![
                "commit",
                "create",
                "--author",
                "Ann",
                "--parent",
                "5073d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c",
                &tree,
            ],
        ),
        (
            "parent with a path",
            vec![
                "commit",
                "create",
                "--author",
                "Ann",
                "--parent",
                "ref:main@sub",
                &tree,
            ],
        ),
        ("show with a path", vec!["commit", "show", "ref:main@sub"]),
    ] {
        assert!(
            run_seg(store.path(), &args).is_err(),
            "{name}: command succeeded"
        );
    }
}
