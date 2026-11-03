//! The collector over git objects (Go: `gc/git_test.go`).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

use crate::fstree;
use crate::gitobj::{self, Kind};
use crate::key::{GIT_SHA1_SIZE, Key};
use crate::packstore;

use super::collector::Cancel;
use super::tests::{
    HOUR, backdate_packs, count_gone, new_test_store, open_collector, put_test_ref, rm_test_ref,
};
use super::{Error, Options};

fn store_git(st: &packstore::Store, kind: Kind, body: &[u8]) -> Key {
    let data = gitobj::encode(kind, body);
    let k = gitobj::key_of(&data).unwrap();
    st.put(k, &data).unwrap();
    k
}

fn git_entry(mode: &str, name: &str, k: Key) -> Vec<u8> {
    let mut b = format!("{mode} {name}\0").into_bytes();
    b.extend_from_slice(&k.git_sha1_sum().unwrap());
    b
}

fn oid_of(k: Key) -> String {
    k.git_sha1_sum()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@example.com")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000");
    cmd
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_command(dir, args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn pipe_git(dir: &Path, stdin: &str, args: &[&str]) -> String {
    let mut child = git_command(dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A repository with what a store has to walk: history with a merge, an annotated
/// tag and a tag of a tag, nested directories, an executable, a symlink, a
/// submodule whose commit is not in the repository, and objects no branch reaches.
/// `None` without a git on PATH.
fn new_real_repo() -> Option<TempDir> {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("no git on PATH: skipping");
        return None;
    }
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let write = |name: &str, content: &str| {
        let p = dir.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    };
    git(dir, &["init", "-q", "-b", "main"]);
    write("README", "readme\n");
    write("src/a/deep.txt", "deep\n");
    write("run.sh", "#!/bin/sh\n");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::os::unix::fs::symlink("README", dir.join("link")).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "first"]);
    git(dir, &["tag", "-a", "-m", "annotated", "v1"]);

    git(dir, &["checkout", "-q", "-b", "side"]);
    write("side.txt", "side\n");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "side"]);
    git(dir, &["checkout", "-q", "main"]);

    write("src/a/deep.txt", "deeper\n");
    git(
        dir,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,module",
        ],
    );
    git(dir, &["add", "src"]);
    git(
        dir,
        &[
            "commit",
            "-q",
            "-m",
            "second",
            "-m",
            "parent 2222222222222222222222222222222222222222\n",
        ],
    );
    git(dir, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    git(dir, &["tag", "-a", "-m", "of a tag", "v2", "v1"]);

    // Objects no reference reaches: a blob, and a commit with a tree of its own.
    let dangling = pipe_git(dir, "only here\n", &["hash-object", "-w", "--stdin"]);
    let tree = pipe_git(
        dir,
        &format!("100644 blob {dangling}\tonly-here\n"),
        &["mktree"],
    );
    pipe_git(dir, "dangling\n", &["commit-tree", &tree]);
    Some(tmp)
}

/// Stores every object of a repository, loose or packed, reachable or not, through
/// the verifying write path, and returns their keys by git id.
fn push_all(st: &packstore::Store, dir: &Path) -> HashMap<String, Key> {
    let mut child = git_command(dir, &["cat-file", "--batch-all-objects", "--batch"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut r = BufReader::new(child.stdout.take().unwrap());
    let mut objects = HashMap::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap() == 0 {
            break;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(f.len(), 3, "cat-file said {line:?}");
        let kind = match f[1] {
            "blob" => Kind::Blob,
            "tree" => Kind::Tree,
            "commit" => Kind::Commit,
            "tag" => Kind::Tag,
            other => panic!("kind {other}"),
        };
        let size: usize = f[2].parse().unwrap();
        let mut content = vec![0u8; size + 1]; // and the newline after it
        r.read_exact(&mut content).unwrap();
        let data = gitobj::encode(kind, &content[..size]);
        let k = gitobj::key_of(&data).unwrap();
        assert_eq!(oid_of(k), f[0], "the store's SHA-1 of a {}", f[1]);
        st.put_verified(k, &data)
            .unwrap_or_else(|e| panic!("{} {}: {e}", f[1], f[0]));
        objects.insert(f[0].to_string(), k);
    }
    assert!(child.wait().unwrap().success());
    objects
}

/// The ids of what git itself reaches from `rev`. A tag's own object is listed
/// apart from what it points at.
fn rev_list(dir: &Path, rev: &str) -> BTreeSet<String> {
    let mut ids: BTreeSet<String> = git(dir, &["rev-list", "--objects", rev])
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect();
    // rev-list may start below a tag: walk down the chain by hand.
    let mut cur = git(dir, &["rev-parse", rev]);
    while git(dir, &["cat-file", "-t", &cur]) == "tag" {
        ids.insert(cur.clone());
        let tag = git(dir, &["cat-file", "tag", &cur]);
        cur = tag
            .lines()
            .next()
            .unwrap()
            .strip_prefix("object ")
            .unwrap()
            .to_string();
    }
    ids
}

fn ids_of(keys: &[Key]) -> BTreeSet<String> {
    keys.iter().map(|k| oid_of(*k)).collect()
}

/// What the store reaches from a branch or a tag is, to the object, what git
/// itself reaches from it.
#[test]
fn git_walks_agree_with_git() {
    let Some(repo) = new_real_repo() else { return };
    let dir = repo.path();
    let ts = new_test_store(1 << 20);
    let objects = push_all(&ts.objects, dir);
    for rev in ["main", "side", "v1", "v2", "HEAD~1"] {
        let id = git(dir, &["rev-parse", rev]);
        let root = objects[&id];
        let want = rev_list(dir, rev);
        let got = fstree::reachable_keys(root, |k| ts.objects.get(k)).unwrap();
        assert_eq!(ids_of(&got), want, "reachable_keys from {rev}");
        let visited =
            fstree::check_complete(root, |k| ts.objects.get(k), |k| ts.objects.has(k), 4).unwrap();
        assert_eq!(ids_of(&visited), want, "check_complete from {rev}");
    }
}

/// Pushing a branch is a reference to its head. Everything git reaches from the
/// head outlives a forced cycle, what git does not reach goes, and the objects
/// that are left can be read back as git wrote them.
#[test]
fn git_branch_stays_live_through_a_cycle() {
    let Some(repo) = new_real_repo() else { return };
    let dir = repo.path();
    let ts = new_test_store(2 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let objects = push_all(&ts.objects, dir);
    let head = git(dir, &["rev-parse", "main"]);
    put_test_ref(&c, &ts.refs, "refs/heads/main", objects[&head]);

    let live = rev_list(dir, "main");
    let dead: Vec<Key> = objects
        .iter()
        .filter(|(id, _)| !live.contains(*id))
        .map(|(_, k)| *k)
        .collect();
    assert!(
        !dead.is_empty(),
        "the repository has no object main does not reach"
    );

    backdate_packs(&ts);
    let stats = c.run(0.0).unwrap();
    assert!(!stats.reaped.is_empty(), "nothing reaped: {stats:?}");
    for id in &live {
        let k = objects[id];
        let data = ts
            .objects
            .get(k)
            .unwrap_or_else(|e| panic!("object {id}, which main reaches: {e}"));
        gitobj::verify(k, &data).unwrap();
    }
    assert!(
        count_gone(&ts.objects, &dead) > 0,
        "no object was collected that main does not reach"
    );
    ts.objects.verify(|| false).unwrap();

    // Dropping the branch makes it all garbage. The first cycle seals what the
    // last one copied, which the grace period then has to be told is old.
    rm_test_ref(&c, &ts.refs, "refs/heads/main", objects[&head]);
    c.run(0.0).unwrap();
    backdate_packs(&ts);
    c.run(0.0).unwrap();
    for id in &live {
        assert_eq!(
            count_gone(&ts.objects, &[objects[id]]),
            1,
            "object {id} outlived the branch"
        );
    }
}

#[test]
fn why_walks_git_objects() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let blob = store_git(&ts.objects, Kind::Blob, b"hello\n");
    let other = store_git(&ts.objects, Kind::Blob, b"not in the tree\n");
    let tree = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", blob));
    let tip = store_git(
        &ts.objects,
        Kind::Commit,
        format!("tree {}\n", oid_of(tree)).as_bytes(),
    );
    put_test_ref(&c, &ts.refs, "main", tip);
    assert_eq!(c.why(blob).unwrap(), ["main"]);
    assert!(c.why(other).unwrap().is_empty());
}

/// One entry calls an object a file and another a directory: `gc why` must see below
/// it, whichever it meets first, or it calls a live object unreferenced.
#[test]
fn why_follows_an_object_also_named_as_a_file() {
    for (first, other) in [("40000", "100644"), ("100644", "40000")] {
        let ts = new_test_store(1 << 20);
        let c = open_collector(&ts, Options::default());
        let below = store_git(&ts.objects, Kind::Blob, b"below\n");
        let sub = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", below));
        let top = store_git(
            &ts.objects,
            Kind::Tree,
            &[git_entry(first, "a", sub), git_entry(other, "b", sub)].concat(),
        );
        put_test_ref(&c, &ts.refs, "main", top);
        assert_eq!(c.why(below).unwrap(), ["main"], "{first} first");
    }
}

#[test]
fn why_does_not_read_git_blobs() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let k = gitobj::key_of(b"not a git object").unwrap();
    ts.objects.put(k, b"not a git object").unwrap();
    let tree = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", k));
    put_test_ref(&c, &ts.refs, "main", tree);
    let other = store_git(&ts.objects, Kind::Blob, b"not in the tree\n");
    assert!(c.why(other).expect("why read a blob").is_empty());
}

/// An object only ever named as a file is never parsed, by git or by the store,
/// and a repository may hold one that would not parse. It gets in through a
/// verifying write and keeps a reference, and it must not stop the sweeps that copy
/// it: a collection refused for the whole store, over bytes nothing has any need to
/// read.
#[test]
fn an_object_named_only_as_a_file_does_not_stop_collection() {
    let ts = new_test_store(2 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let put = |kind: Kind, body: &[u8]| -> Key {
        let data = gitobj::encode(kind, body);
        let k = gitobj::key_of(&data).unwrap();
        ts.objects
            .put_verified(k, &data)
            .expect("put_verified of a framed object");
        k
    };
    // git reads a commit like this as a file, and would not as a commit.
    let unparsable = put(Kind::Commit, b"this is no commit");
    let top = put(Kind::Tree, &git_entry("100644", "f", unparsable));
    let tip = put(Kind::Commit, format!("tree {}\n", oid_of(top)).as_bytes());
    let dead: Vec<Key> = (0..20)
        .map(|i| put(Kind::Blob, format!("{i:0400}").as_bytes()))
        .collect();
    put_test_ref(&c, &ts.refs, "main", tip);
    for i in 0..2 {
        backdate_packs(&ts);
        c.run(0.0).unwrap_or_else(|e| panic!("cycle {i}: {e}"));
    }
    ts.objects.get(unparsable).expect("what a reference keeps");
    assert!(
        count_gone(&ts.objects, &dead) > 0,
        "no garbage was collected"
    );
}

/// What a walk has to read, it has to be able to read. That is checked before a
/// reference is accepted, so no accepted reference can stop a mark.
#[test]
fn prepare_ref_refuses_what_cannot_be_walked() {
    type Link = fn(&packstore::Store, Key) -> Key;
    let cases: [(&str, Link); 4] = [
        ("the root", |_, bad| bad),
        ("a commit's tree", |st, bad| {
            store_git(
                st,
                Kind::Commit,
                format!("tree {}\n", oid_of(bad)).as_bytes(),
            )
        }),
        ("a directory entry", |st, bad| {
            store_git(st, Kind::Tree, &git_entry("40000", "d", bad))
        }),
        ("named as a file and as a directory", |st, bad| {
            store_git(
                st,
                Kind::Tree,
                &[git_entry("100644", "a", bad), git_entry("40000", "b", bad)].concat(),
            )
        }),
    ];
    for (name, link) in cases {
        let ts = new_test_store(1 << 20);
        let c = open_collector(&ts, Options::default());
        let data = gitobj::encode(Kind::Commit, b"this is no commit");
        let bad = gitobj::key_of(&data).unwrap();
        ts.objects.put_verified(bad, &data).unwrap();
        let root = link(&ts.objects, bad);
        let Err(err) = c.prepare_ref(root) else {
            panic!("{name}: an object that cannot be walked was accepted");
        };
        assert!(
            err.to_string().contains("gitobj: malformed object"),
            "{name}: {err}"
        );
    }
}

/// A blob's tree entry says what it is, so the mark does not read it: a cycle
/// costs what the trees and commits cost, not every version of every file. The
/// payload here is no git object at all, and reading it would fail the mark.
#[test]
fn mark_does_not_read_git_blobs() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let unreadable = b"not a git object";
    let k = gitobj::key_of(unreadable).unwrap();
    ts.objects.put(k, unreadable).unwrap();
    let tree = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", k));
    let tip = store_git(
        &ts.objects,
        Kind::Commit,
        format!("tree {}\n", oid_of(tree)).as_bytes(),
    );
    let dead = c
        .unreachable_from(&[tip], &[k])
        .expect("the mark read a blob its tree entry named");
    assert!(dead.is_empty(), "a blob the tree names is not marked");
}

/// One entry calls an object a file and another calls it a directory. Whichever
/// the mark meets first, what lies below it stays: git reads through the directory
/// entry.
#[test]
fn mark_follows_an_object_also_named_as_a_file() {
    for (name, first, other) in [
        ("file sorted first", "100644", "40000"),
        ("directory sorted first", "40000", "100644"),
    ] {
        let ts = new_test_store(1 << 20);
        let c = open_collector(&ts, Options::default());
        let below = store_git(&ts.objects, Kind::Blob, b"below\n");
        let sub = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", below));
        let top = store_git(
            &ts.objects,
            Kind::Tree,
            &[git_entry(first, "a", sub), git_entry(other, "b", sub)].concat(),
        );
        let dead = c.unreachable_from(&[top], &[sub, below]).unwrap();
        assert!(dead.is_empty(), "{name}: not marked: {dead:?}");
    }
    // The same across two roots: the first marks it a file, the second walks it.
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let below = store_git(&ts.objects, Kind::Blob, b"below\n");
    let sub = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", below));
    let as_file = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "a", sub));
    let as_dir = store_git(&ts.objects, Kind::Tree, &git_entry("40000", "b", sub));
    let dead = c.unreachable_from(&[as_file, as_dir], &[below]).unwrap();
    assert!(dead.is_empty(), "across roots: not marked: {dead:?}");
}

/// A shared interior object is read once however many links name it.
#[test]
fn mark_counts_a_shared_git_tree_once() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let blob = store_git(&ts.objects, Kind::Blob, b"x\n");
    let sub = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", blob));
    let top = store_git(
        &ts.objects,
        Kind::Tree,
        &[git_entry("40000", "a", sub), git_entry("40000", "b", sub)].concat(),
    );
    let live = c.core.mark_live(Cancel::NONE, &[top, top]).unwrap();
    assert_eq!(live.marked(), 3);
}

#[test]
fn prepare_ref_notices_a_missing_git_object() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let k = gitobj::key_of(&gitobj::encode(Kind::Blob, b"never stored\n")).unwrap();
    let tree = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", k));
    let tip = store_git(
        &ts.objects,
        Kind::Commit,
        format!("tree {}\n", oid_of(tree)).as_bytes(),
    );
    let Err(err) = c.prepare_ref(tip) else {
        panic!("a missing blob was accepted");
    };
    assert!(err.to_string().contains(&k.to_string()), "{err}");
}

/// A submodule's commit is in another repository: a branch that holds one is
/// complete without it, as it is for git.
#[test]
fn prepare_ref_does_not_require_a_submodule() {
    let ts = new_test_store(1 << 20);
    let c = open_collector(&ts, Options::default());
    let mut module = [0u8; GIT_SHA1_SIZE];
    module[0] = 0x11;
    let tree = store_git(
        &ts.objects,
        Kind::Tree,
        &git_entry("160000", "mod", Key::new_git_sha1(module)),
    );
    let tip = store_git(
        &ts.objects,
        Kind::Commit,
        format!("tree {}\n", oid_of(tree)).as_bytes(),
    );
    c.prepare_ref(tip).expect("prepare_ref").commit();
}

/// A git key does not say it is a blob, so only a verified mark lets the cycle go on
/// past a bad one. A git store gets the same as any other: one bad file costs one pack
/// (Go: `TestABadGitBlobDoesNotStopCollection`).
#[test]
fn a_bad_git_blob_does_not_stop_collection() {
    let ts = new_test_store(2 << 10);
    let c = open_collector(
        &ts,
        Options {
            grace: HOUR,
            ..Options::default()
        },
    );
    let data = gitobj::encode(Kind::Blob, b"hello\n");
    let blob = gitobj::key_of(&data).unwrap();
    ts.objects.put_verified(blob, &data).unwrap();
    let tree = store_git(&ts.objects, Kind::Tree, &git_entry("100644", "f", blob));
    let tip = store_git(
        &ts.objects,
        Kind::Commit,
        format!("tree {}\n", oid_of(tree)).as_bytes(),
    );
    let dead: Vec<Key> = (0..40)
        .map(|i| store_git(&ts.objects, Kind::Blob, format!("{i:0400}").as_bytes()))
        .collect();
    put_test_ref(&c, &ts.refs, "main", tip);
    // The blob is the first record of the first pack.
    let p = ts.dir.join("packstore/0000000000000001.seg");
    let mut b = fs::read(&p).unwrap();
    b[8 + crate::amberpack::REC_HEADER_SIZE] ^= 0x40;
    fs::write(&p, b).unwrap();
    backdate_packs(&ts);
    let err = c.run(0.0).expect_err("a bad record passed");
    let Error::Skipped { stats, .. } = &err else {
        panic!("not a skip: {err}");
    };
    assert_eq!(stats.skipped.len(), 1);
    assert_eq!(stats.skipped[0].key, blob);
    assert!(count_gone(&ts.objects, &dead) > 0, "collection stopped");
    ts.objects.put_verified(blob, &data).unwrap();
    backdate_packs(&ts);
    c.run(0.0).expect("after the repair");
    fstree::check_complete(
        tip,
        |k| ts.objects.get_verified(k),
        |k| ts.objects.has(k),
        1,
    )
    .expect("after a cycle");
}
