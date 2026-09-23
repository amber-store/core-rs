//! End-to-end tests of the `amber-store` CLI example: a port of Go
//! `cmd/amber-store/e2e_test.go`'s pin..HEAD delta
//! (`TestE2E_RefSetChecksCompleteness`, `TestE2E_RefLifecycle`,
//! `TestE2E_GC`) plus the pre-existing `TestE2E_MissingStoreFlag`, which
//! had no Rust counterpart yet, `TestE2E_Commit` (Go PR #12) and
//! `TestE2E_RefExpect` (Go PR #13). The Go
//! unit tests of `parseIdentity` and `renderCommit` are covered through the
//! CLI in `commit_identity_and_rendering`: the example carries no unit
//! tests. Go PR #15 added `TestE2E_CommitInsideADirectory`,
//! `TestE2E_CommitKeyedByTheOldRuleIsRefused` and
//! `TestE2E_CommitUnderARegularFileEntryIsRefused`, which build through the
//! library what no command can, and `TestRenderCommitConflicted` /
//! `TestIdentityLineWithoutNameOrEmail`, covered here by `commit show` of
//! commits stored through the library. Go drives `newApp()` in-process; here
//! each case spawns the compiled example binary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use tempfile::TempDir;

use amber_store_core::commit::{Commit, Identity};
use amber_store_core::fstree::{self, Entry};
use amber_store_core::key::{Key, Type};
use amber_store_core::{packstore, tarextract};

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

/// Runs the CLI with only `--store <dir>` ahead of `rest`, as the Go tests
/// of commits inside directories do; the output comes back trimmed.
fn run_store(store: &Path, rest: &[&str]) -> Result<String, String> {
    let mut args = vec!["--store".to_string(), store.display().to_string()];
    args.extend(rest.iter().map(|s| s.to_string()));
    run_app(&args).map(|out| out.trim().to_string())
}

/// Opens the store's objects through the library, where the CLI keeps them.
/// No command builds a tree that holds a commit (ingest reads a filesystem,
/// which has none), so the tests do, as Go's do.
fn open_objects(store: &Path) -> packstore::Store {
    packstore::Store::open(store.join("packstore")).expect("open the packstore")
}

fn parse_key(printed: &str) -> Key {
    Key::parse(&hex::decode(printed).expect("hex key")).expect("canonical key")
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
    let c2 = create(&[
        "--parent",
        "ref:main",
        "--change-id",
        "00ff10",
        "-m",
        "second",
        &root2,
    ]);
    // A change id that is given has to be hex, and not empty; the error text
    // is Go's, encoding/hex's included.
    for (bad, want) in [
        ("xyz", "--change-id: encoding/hex: invalid byte: U+0078 'x'"),
        ("abc", "--change-id: encoding/hex: odd length hex string"),
        (
            "",
            "--change-id: empty; leave the flag out for a commit without a change id",
        ),
    ] {
        let err = run_seg(
            store.path(),
            &[
                "commit",
                "create",
                "--author",
                "Ann <ann@example.com>",
                "--change-id",
                bad,
                "-m",
                "bad",
                &root2,
            ],
        )
        .expect_err("commit create accepted a bad change id");
        assert!(err.contains(want), "change id {bad:?}: {err}");
    }
    assert!(c1 != c2 && c2 != root2, "c1 {c1}, c2 {c2}, root2 {root2}");
    assert_eq!(run(&["ref", "get", "main"]), c2, "ref get main");

    let show = run(&["commit", "show", "ref:main"]);
    for want in [
        format!("commit {c2}"),
        format!("tree {root2}"),
        format!("parent {c1}"),
        "change-id 00ff10".to_string(),
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

// Port of Go TestE2E_RefExpect, with the messages asserted as well and one
// case Go's test cannot tell apart: a misplaced --expect carrying the RIGHT
// key, which must fail the argument count rather than be honoured.
#[test]
fn ref_expect() {
    let store = TempDir::new().unwrap();
    let store_s = store.path().display().to_string();
    let ingest = |extra: &str| -> String {
        let src = TempDir::new().unwrap();
        write_fixture(src.path());
        fs::write(src.path().join("extra.txt"), extra).unwrap();
        let src_s = src.path().display().to_string();
        match run_app(&["--store", &store_s, "ingest", "--no-progress", &src_s]) {
            Ok(out) => out.trim().to_string(),
            Err(e) => panic!("ingest: {e}"),
        }
    };
    let (root1, root2) = (ingest("one"), ingest("two"));
    let (root1, root2) = (root1.as_str(), root2.as_str());
    let rf = |args: &[&str]| -> Result<String, String> {
        let mut all = vec!["--store", store_s.as_str(), "ref"];
        all.extend_from_slice(args);
        run_app(&all).map(|out| out.trim().to_string())
    };
    // The command must fail, and say why.
    let refused = |args: &[&str], why: &str, what: &str| match rf(args) {
        Ok(_) => panic!("{what}"),
        Err(e) => assert!(
            e.contains(why),
            "{what}: failed, but with {e:?}; want {why:?}"
        ),
    };
    let want_at = |want: &str| {
        let got = rf(&["get", "r"]);
        assert_eq!(got.as_deref(), Ok(want), "ref get r");
    };

    if let Err(e) = rf(&["set", "--expect", "none", "r", root1]) {
        panic!("create with --expect none: {e}");
    }
    refused(
        &["set", "--expect", "none", "r", root2],
        "already exists: refstore: reference is not at the expected key",
        "--expect none overwrote an existing reference",
    );
    want_at(root1);
    refused(
        &["set", "--expect", root2, "r", root2],
        "does not point at",
        "a stale --expect moved the reference",
    );
    want_at(root1);
    // A flag after the positionals is not parsed as a flag; it must fail
    // rather than turn into an unconditional set.
    refused(
        &["set", "r", root2, "--expect", root2],
        "ref set requires NAME KEY arguments, got 4",
        "a misplaced --expect was accepted",
    );
    want_at(root1);
    refused(
        &["set", "r", root2, "--expect", root1],
        "ref set requires NAME KEY arguments, got 4",
        "a misplaced --expect with the current key was honoured",
    );
    want_at(root1);
    // An empty expectation, a script's unset variable, must fail as well.
    refused(
        &["set", "--expect", "", "r", root2],
        "--expect is empty",
        "an empty --expect was accepted by ref set",
    );
    want_at(root1);
    refused(
        &["set", "--expect", "zz", "r", root2],
        "--expect: ",
        "a malformed --expect was accepted",
    );
    want_at(root1);
    if let Err(e) = rf(&["set", "--expect", root1, "r", root2]) {
        panic!("set with the right --expect: {e}");
    }
    want_at(root2);

    refused(
        &["rm", "--expect", "none", "r"],
        "a delete cannot expect the reference to be absent",
        "ref rm accepted --expect none",
    );
    refused(
        &["rm", "--expect", "", "r"],
        "--expect is empty",
        "an empty --expect was accepted by ref rm",
    );
    refused(
        &["rm", "r", "--expect", root2],
        "ref rm requires exactly one NAME argument, got 3",
        "ref rm honoured a misplaced --expect",
    );
    want_at(root2);
    refused(
        &["rm", "--expect", root1, "r"],
        "does not point at",
        "a stale --expect deleted the reference",
    );
    want_at(root2);
    if let Err(e) = rf(&["rm", "--expect", root2, "r"]) {
        panic!("rm with the right --expect: {e}");
    }
    assert!(rf(&["get", "r"]).is_err(), "the reference survived ref rm");
    refused(
        &["set", "--expect", root1, "gone", root1],
        "does not exist, expected it at",
        "--expect KEY created a reference that did not exist",
    );
    refused(
        &["rm", "--expect", root1, "gone"],
        "does not exist, expected it at",
        "ref rm --expect deleted a reference that did not exist",
    );
    assert!(rf(&["get", "gone"]).is_err());

    // A NAME that begins with a dash is a flag, to Go's parser and to this
    // one; after `--` it is a name.
    refused(
        &["set", "-lead", root1],
        "unexpected argument",
        "a name that looks like a flag was accepted",
    );
    if let Err(e) = rf(&["set", "--", "-lead", root1]) {
        panic!("ref set -- -lead: {e}");
    }
    assert!(rf(&["list"]).unwrap().contains("-lead"));
    refused(
        &["rm", "-lead"],
        "unexpected argument",
        "ref rm took a flag for a name",
    );
    if let Err(e) = rf(&["rm", "--", "-lead"]) {
        panic!("ref rm -- -lead: {e}");
    }
    assert!(!rf(&["list"]).unwrap().contains("-lead"));
}

// ---------------------------------------------------------------------------
// Commits inside directories, the footprint rule, conflicts (Go PR #15).
// ---------------------------------------------------------------------------

/// A directory entry may hold a commit. Every file operation reads through it
/// to the commit's tree: the commit object itself is skipped (Go:
/// `TestE2E_CommitInsideADirectory`).
#[test]
fn commit_inside_a_directory() {
    let src = TempDir::new().unwrap();
    write_fixture(src.path()); // a.txt, sub/b.txt, link
    let store = TempDir::new().unwrap();
    let src_s = src.path().display().to_string();
    let run = |args: &[&str]| -> String {
        run_store(store.path(), args).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    };
    let root = run(&["ingest", "--no-progress", &src_s]);
    let author = ["--author", "Ann <ann@example.com>"];
    let vendored = run(&[
        "commit", "create", author[0], author[1], "-m", "vendored", &root,
    ]);

    let ck = parse_key(&vendored);
    let objects = open_objects(store.path());
    let main_blob = fstree::encode_blob(b"package main");
    let holder = fstree::encode_dir_leaf(&[
        Entry {
            name: b"main.go".to_vec(),
            mode: 0o100644,
            content_key: main_blob.key.as_bytes().to_vec(),
            ..Default::default()
        },
        Entry {
            name: b"vendor".to_vec(),
            mode: 0o040755,
            content_key: ck.as_bytes().to_vec(),
            ..Default::default()
        },
    ])
    .unwrap();
    for o in [&main_blob, &holder] {
        objects.put(o.key, &o.bytes).unwrap();
    }
    objects.close().unwrap();
    let top = holder.key.to_string();

    for (spec, want) in [
        (top.clone(), "vendor"),
        (format!("{top}/vendor"), "a.txt"),
        (format!("{top}/vendor/sub"), "b.txt"),
    ] {
        let out = run(&["ls", &spec]);
        assert!(out.contains(want), "ls {spec} output {out:?} lacks {want}");
    }

    // The archive holds the commit's tree under vendor/, as a plain directory.
    let tmp = TempDir::new().unwrap();
    let tar_path = tmp.path().join("top.tar");
    run(&["export", "-o", &tar_path.display().to_string(), &top]);
    let unpacked = tmp.path().join("unpacked");
    tarextract::extract(&mut fs::File::open(&tar_path).unwrap(), &unpacked).unwrap();
    let dest = tmp.path().join("restored");
    run(&["restore", &top, &dest.display().to_string()]);
    for dir in [&unpacked, &dest] {
        for (name, want) in [
            ("main.go", "package main"),
            ("vendor/a.txt", "alpha"),
            ("vendor/sub/b.txt", "beta"),
        ] {
            let got = fs::read_to_string(dir.join(name))
                .unwrap_or_else(|e| panic!("{}: {e}", dir.join(name).display()));
            assert_eq!(got, want, "{}", dir.join(name).display());
        }
        assert!(dir.join("vendor").is_dir());
    }

    // TREE may name a commit through such an entry: the new commit records
    // that commit's tree, not the commit.
    let regraft = run(&[
        "commit",
        "create",
        author[0],
        author[1],
        "-m",
        "regraft",
        &format!("{top}/vendor"),
    ]);
    let show = run(&["commit", "show", &regraft]);
    assert!(
        show.contains(&format!("tree {root}")),
        "commit show {show:?} does not record the vendored commit's tree {root}"
    );
    // So it does when it is named directly.
    let again = run(&[
        "commit", "create", author[0], author[1], "-m", "again", &vendored,
    ]);
    let show = run(&["commit", "show", &again]);
    assert!(show.contains(&format!("tree {root}")), "{show:?}");
}

/// A commit keyed by the first release's rule, its own bytes alone, is not a
/// commit any more. Nothing may be built on it: not a child commit, not a
/// reference, not a listing. `commit show` still prints it, which is how its
/// tree is found again (Go: `TestE2E_CommitKeyedByTheOldRuleIsRefused`).
#[test]
fn commit_keyed_by_the_old_rule_is_refused() {
    let src = TempDir::new().unwrap();
    write_fixture(src.path());
    let store = TempDir::new().unwrap();
    let src_s = src.path().display().to_string();
    let root = run_store(store.path(), &["ingest", "--no-progress", &src_s]).unwrap();
    let id = Identity {
        name: "Ann".into(),
        when: 1,
        ..Default::default()
    };
    let (good, data) = Commit {
        tree: parse_key(&root),
        parents: Vec::new(),
        author: id.clone(),
        committer: id,
        message: "from the first release".into(),
        signature: Vec::new(),
        public_key: Vec::new(),
        change_id: Vec::new(),
        conflict_terms: Vec::new(),
        conflict_labels: Vec::new(),
    }
    .object()
    .unwrap();
    let old = Key::new(Type::Commit, data.len() as u64, &data);
    assert_ne!(old, good);
    let objects = open_objects(store.path());
    objects.put(old, &data).unwrap(); // put trusts its caller, as it did then
    objects.close().unwrap();
    let old = old.to_string();

    let out = run_store(store.path(), &["commit", "show", &old])
        .expect("commit show should still print the commit, and so its tree");
    assert!(out.contains(&format!("tree {root}")), "{out:?}");

    let rule = format!(
        "length field {own} is not the commit's footprint {} (its own {own} bytes plus its trees); a commit keyed by an older rule has to be created again",
        good.length(),
        own = data.len()
    );
    for args in [
        vec![
            "commit",
            "create",
            "--author",
            "Ann <ann@example.com>",
            "--parent",
            &old,
            "-m",
            "child",
            &root,
        ],
        vec!["ref", "set", "old", &old],
        vec!["ls", &old],
    ] {
        let err = run_store(store.path(), &args)
            .expect_err("a commit keyed by the old rule was built on");
        assert!(
            err.contains(&rule),
            "{args:?}: want an error that names the footprint rule, got {err}"
        );
    }
    // commit create names the parent it refuses, as Go does.
    let err = run_store(
        store.path(),
        &[
            "commit",
            "create",
            "--author",
            "Ann <ann@example.com>",
            "--parent",
            &old,
            "-m",
            "child",
            &root,
        ],
    )
    .unwrap_err();
    assert!(
        err.contains(&format!("parent {old}: fstree: Commit {old}: length field")),
        "{err}"
    );
}

/// Only a directory entry may hold a commit. Under an entry of another type
/// it is a malformed tree, which path resolution refuses (Go:
/// `TestE2E_CommitUnderARegularFileEntryIsRefused`).
#[test]
fn commit_under_a_regular_file_entry_is_refused() {
    let src = TempDir::new().unwrap();
    write_fixture(src.path());
    let store = TempDir::new().unwrap();
    let src_s = src.path().display().to_string();
    let root = run_store(store.path(), &["ingest", "--no-progress", &src_s]).unwrap();
    let vendored = run_store(
        store.path(),
        &[
            "commit",
            "create",
            "--author",
            "Ann <ann@example.com>",
            "-m",
            "vendored",
            &root,
        ],
    )
    .unwrap();
    let holder = fstree::encode_dir_leaf(&[Entry {
        name: b"odd".to_vec(),
        mode: 0o100644,
        content_key: parse_key(&vendored).as_bytes().to_vec(),
        ..Default::default()
    }])
    .unwrap();
    let objects = open_objects(store.path());
    objects.put(holder.key, &holder.bytes).unwrap();
    objects.close().unwrap();

    let err = run_store(store.path(), &["ls", &format!("{}/odd/sub", holder.key)])
        .expect_err("ls through a regular-file entry that holds a commit");
    assert!(
        err.contains("\"odd\" holds a commit but is not a directory entry"),
        "{err}"
    );
}

/// Go's `TestRenderCommitConflicted` and `TestIdentityLineWithoutNameOrEmail`,
/// byte for byte, through `commit show`: no command creates a conflicted
/// commit or a nameless identity, so the commits are stored through the
/// library. `commit show` prints a commit without looking at its trees or
/// parents, so fabricated keys serve, as in Go.
#[test]
fn commit_show_conflicted_and_nameless() {
    let store = TempDir::new().unwrap();
    // Any command creates the store's layout; ref list is the cheapest.
    run_store(store.path(), &["ref", "list"]).unwrap();
    let hash = |first: u8| {
        let mut h = [0u8; 32];
        h[0] = first;
        h
    };
    let tree = Key::new(Type::DirLeaf, 1, &[0x80]);
    let remove = Key::new_from_hash(Type::DirLeaf, 300, hash(1));
    let add = Key::new_from_hash(Type::DirNode, 70000, hash(2));
    let parent = Key::new_from_hash(Type::Commit, 7, hash(1));
    let when = 1_767_323_045_000_000_000;
    let conflicted = Commit {
        tree,
        parents: vec![parent],
        author: Identity {
            name: "Ann".into(),
            email: String::new(),
            when,
            tz_offset: 60,
        },
        committer: Identity {
            name: String::new(),
            email: "bot@example.com".into(),
            when,
            tz_offset: 60,
        },
        message: "m".into(),
        signature: Vec::new(),
        public_key: Vec::new(),
        change_id: vec![0xab, 0xcd],
        conflict_terms: vec![remove, add],
        conflict_labels: vec!["ours".into(), String::new(), "theirs".into()],
    };
    let nobody = Commit {
        tree,
        parents: Vec::new(),
        author: Identity::default(),
        committer: Identity::default(),
        message: String::new(),
        signature: Vec::new(),
        public_key: Vec::new(),
        change_id: Vec::new(),
        conflict_terms: Vec::new(),
        conflict_labels: Vec::new(),
    };
    let (ck, cbytes) = conflicted.object().unwrap();
    let (nk, nbytes) = nobody.object().unwrap();
    let objects = open_objects(store.path());
    objects.put(ck, &cbytes).unwrap();
    objects.put(nk, &nbytes).unwrap();
    objects.close().unwrap();

    // run_app, not run_store: the output is compared untrimmed.
    let show = |k: Key| {
        run_app(&[
            "--store".to_string(),
            store.path().display().to_string(),
            "commit".to_string(),
            "show".to_string(),
            k.to_string(),
        ])
        .unwrap()
    };
    assert_eq!(
        show(ck),
        format!(
            "commit {ck}\n\
             tree {tree}\n\
             conflict-remove {remove}\n\
             conflict-add {add}\n\
             conflict-label 0 ours\n\
             conflict-label 2 theirs\n\
             parent {parent}\n\
             change-id abcd\n\
             author Ann 2026-01-02T04:04:05+01:00\n\
             committer <bot@example.com> 2026-01-02T04:04:05+01:00\n\
             \n    m\n"
        )
    );
    // An identity may name nobody at all: no stray space before the time.
    assert_eq!(
        show(nk),
        format!(
            "commit {nk}\ntree {tree}\nauthor 1970-01-01T00:00:00Z\ncommitter 1970-01-01T00:00:00Z\n"
        )
    );
}
