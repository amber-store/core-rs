//! Walks over git objects (Go: `fstree/git_test.go`).

use std::collections::HashMap;
use std::sync::Mutex;

use super::read::{ChildKeysError, MissingObjectError, WalkError};
use super::{Entry, Link, check_complete, child_keys, dir_of, encode_blob, encode_dir_leaf};
use super::{link_to, links, reachable_keys};
use crate::gitobj::{self, Kind};
use crate::key::{Key, Type};

type Err = &'static str;

#[derive(Clone)]
struct Obj {
    key: Key,
    bytes: Vec<u8>,
}

fn git_object(kind: Kind, body: &[u8]) -> Obj {
    let bytes = gitobj::encode(kind, body);
    Obj {
        key: gitobj::key_of(&bytes).unwrap(),
        bytes,
    }
}

fn oid(k: Key) -> String {
    k.git_sha1_sum()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn tree_entry(mode: &str, name: &str, k: Key) -> Vec<u8> {
    let mut b = format!("{mode} {name}\0").into_bytes();
    b.extend_from_slice(&k.git_sha1_sum().unwrap());
    b
}

/// One commit on top of another, the tip's tree holding a file, a subdirectory
/// with a file of its own, and a submodule whose commit is not here.
struct GitRepo {
    blob: Obj,
    sub_blob: Obj,
    sub: Obj,
    tree: Obj,
    parent: Obj,
    tip: Obj,
    tag: Obj,
}

impl GitRepo {
    fn new() -> GitRepo {
        let blob = git_object(Kind::Blob, b"hello\n");
        let sub_blob = git_object(Kind::Blob, b"below\n");
        let sub = git_object(Kind::Tree, &tree_entry("100644", "f", sub_blob.key));
        let submodule = git_object(Kind::Commit, format!("tree {}\n", oid(sub.key)).as_bytes());
        let tree = git_object(
            Kind::Tree,
            &[
                tree_entry("100644", "a", blob.key),
                tree_entry("40000", "dir", sub.key),
                tree_entry("160000", "mod", submodule.key),
            ]
            .concat(),
        );
        let parent = git_object(
            Kind::Commit,
            format!(
                "tree {}\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\n\nfirst\n",
                oid(sub.key)
            )
            .as_bytes(),
        );
        let tip = git_object(
            Kind::Commit,
            format!(
                "tree {}\nparent {}\nauthor a <a@b> 2 +0000\ncommitter a <a@b> 2 +0000\n\nsecond\n",
                oid(tree.key),
                oid(parent.key)
            )
            .as_bytes(),
        );
        let tag = git_object(
            Kind::Tag,
            format!("object {}\ntype commit\ntag v1\n\nm\n", oid(tip.key)).as_bytes(),
        );
        GitRepo {
            blob,
            sub_blob,
            sub,
            tree,
            parent,
            tip,
            tag,
        }
    }

    fn all(&self) -> Vec<Obj> {
        vec![
            self.blob.clone(),
            self.sub_blob.clone(),
            self.sub.clone(),
            self.tree.clone(),
            self.parent.clone(),
            self.tip.clone(),
            self.tag.clone(),
        ]
    }
}

/// An in-memory store that remembers what it was asked to read.
struct Mem {
    objs: HashMap<Key, Vec<u8>>,
    read: Mutex<HashMap<Key, usize>>,
}

impl Mem {
    fn of(objs: &[Obj]) -> Mem {
        Mem {
            objs: objs.iter().map(|o| (o.key, o.bytes.clone())).collect(),
            read: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, k: Key) -> Result<Vec<u8>, Err> {
        *self.read.lock().unwrap().entry(k).or_default() += 1;
        self.objs.get(&k).cloned().ok_or("not in the store")
    }

    fn has(&self, k: Key) -> Result<bool, Err> {
        Ok(self.objs.contains_key(&k))
    }

    fn reads(&self, k: Key) -> usize {
        self.read.lock().unwrap().get(&k).copied().unwrap_or(0)
    }
}

fn sorted(mut keys: Vec<Key>) -> Vec<Key> {
    keys.sort();
    keys
}

fn keys_of(objs: &[Obj]) -> Vec<Key> {
    objs.iter().map(|o| o.key).collect()
}

#[test]
fn child_keys_of_git_objects() {
    let r = GitRepo::new();
    let cases: [(&str, &Obj, Vec<Key>); 4] = [
        ("blob", &r.blob, vec![]),
        ("tree: no submodule", &r.tree, vec![r.blob.key, r.sub.key]),
        (
            "commit: tree, then parents",
            &r.tip,
            vec![r.tree.key, r.parent.key],
        ),
        ("tag", &r.tag, vec![r.tip.key]),
    ];
    for (name, obj, want) in cases {
        assert_eq!(child_keys(obj.key, &obj.bytes).unwrap(), want, "{name}");
    }
}

#[test]
fn child_keys_of_a_malformed_git_object() {
    let r = GitRepo::new();
    let err = child_keys(r.tip.key, b"not a git object").unwrap_err();
    assert!(matches!(err, ChildKeysError::DecodeGit { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        format!(
            "fstree: decoding git object {}: gitobj: malformed object: unknown kind",
            r.tip.key
        )
    );
}

#[test]
fn links_say_which_children_are_leaves() {
    let r = GitRepo::new();
    assert_eq!(
        links(r.tree.key, &r.tree.bytes).unwrap(),
        vec![
            Link {
                key: r.blob.key,
                leaf: true
            },
            Link {
                key: r.sub.key,
                leaf: false
            }
        ]
    );

    let blob = encode_blob(b"x");
    let dir = encode_dir_leaf(&[]).unwrap();
    let leaf = encode_dir_leaf(&[
        Entry {
            name: b"d".to_vec(),
            mode: 0o040755,
            content_key: dir.key.as_bytes().to_vec(),
            ..Default::default()
        },
        Entry {
            name: b"f".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        },
    ])
    .unwrap();
    assert_eq!(
        links(leaf.key, &leaf.bytes).unwrap(),
        vec![
            Link {
                key: dir.key,
                leaf: false
            },
            Link {
                key: blob.key,
                leaf: true
            }
        ]
    );
}

#[test]
fn a_git_root_has_to_be_read() {
    let r = GitRepo::new();
    assert!(
        !link_to(r.blob.key).leaf,
        "a git key alone said it is a leaf"
    );
    assert!(link_to(Key::new(Type::Blob, 1, b"x")).leaf);
}

#[test]
fn reachable_keys_walks_git() {
    let r = GitRepo::new();
    let all = r.all();
    let mem = Mem::of(&all);
    let got = reachable_keys(r.tag.key, |k| mem.get(k)).unwrap();
    assert_eq!(got[0], r.tag.key, "root is not first");
    assert_eq!(sorted(got), sorted(keys_of(&all)));
    assert_eq!(mem.reads(r.blob.key), 0, "a tree entry says what a blob is");
    assert_eq!(mem.reads(r.sub_blob.key), 0);
}

#[test]
fn reachable_keys_reads_a_git_root() {
    let r = GitRepo::new();
    let mem = Mem::of(std::slice::from_ref(&r.blob));
    let got = reachable_keys(r.blob.key, |k| mem.get(k)).unwrap();
    assert_eq!(got, vec![r.blob.key]);
    assert_eq!(mem.reads(r.blob.key), 1);
}

#[test]
fn reachable_keys_does_not_read_a_blob_root() {
    let b = encode_blob(b"x");
    let got = reachable_keys(b.key, |_| -> Result<Vec<u8>, Err> {
        Err("must not be read")
    });
    assert_eq!(got.unwrap(), vec![b.key]);
}

#[test]
fn check_complete_walks_git() {
    let r = GitRepo::new();
    let all = r.all();
    let mem = Mem::of(&all);
    let visited = check_complete(r.tag.key, |k| mem.get(k), |k| mem.has(k), 4).unwrap();
    assert_eq!(visited[0], r.tag.key, "root is not first");
    assert_eq!(sorted(visited), sorted(keys_of(&all)));
    assert_eq!(mem.reads(r.blob.key), 0, "has says all that is needed");
    assert_eq!(mem.reads(r.sub_blob.key), 0);
}

#[test]
fn check_complete_notices_a_missing_git_object() {
    let r = GitRepo::new();
    let without =
        |gone: Key| -> Vec<Obj> { r.all().into_iter().filter(|o| o.key != gone).collect() };
    // A blob: the walk knows it for a leaf and names it.
    let mem = Mem::of(&without(r.sub_blob.key));
    let err = check_complete(r.tag.key, |k| mem.get(k), |k| mem.has(k), 4).unwrap_err();
    assert!(
        matches!(err, WalkError::Missing(MissingObjectError { key }) if key == r.sub_blob.key),
        "{err}"
    );
    // Anything else is a failed read.
    for gone in [r.parent.key, r.sub.key, r.tree.key, r.tip.key] {
        let mem = Mem::of(&without(gone));
        assert!(check_complete(r.tag.key, |k| mem.get(k), |k| mem.has(k), 4).is_err());
    }
}

/// One entry calls an object a file and another calls the same object a
/// directory. A walk that took the first word for it and pruned on the second
/// would pass over what lies below, and a sweep would lose it while git reads it
/// through the directory entry.
#[test]
fn a_second_link_can_still_make_an_object_interior() {
    let r = GitRepo::new();
    for (name, first, second) in [
        ("file first", "100644", "40000"),
        ("directory first", "40000", "100644"),
    ] {
        let top = git_object(
            Kind::Tree,
            &[
                tree_entry(first, "a", r.sub.key),
                tree_entry(second, "b", r.sub.key),
            ]
            .concat(),
        );
        let objs = [top.clone(), r.sub.clone(), r.sub_blob.clone()];
        let want = sorted(keys_of(&objs));
        let mem = Mem::of(&objs);
        assert_eq!(
            sorted(reachable_keys(top.key, |k| mem.get(k)).unwrap()),
            want,
            "{name}"
        );
        assert_eq!(
            sorted(check_complete(top.key, |k| mem.get(k), |k| mem.has(k), 4).unwrap()),
            want,
            "{name}"
        );
        // And it notices when what lies below is gone.
        let mem = Mem::of(&[top.clone(), r.sub.clone()]);
        let err = check_complete(top.key, |k| mem.get(k), |k| mem.has(k), 4).unwrap_err();
        assert!(
            matches!(err, WalkError::Missing(MissingObjectError { key }) if key == r.sub_blob.key),
            "{name}: {err}"
        );
    }
}

/// The same interior object named at two levels is read once.
#[test]
fn walks_read_each_git_object_once() {
    let r = GitRepo::new();
    let top = git_object(
        Kind::Tree,
        &[
            tree_entry("40000", "a", r.sub.key),
            tree_entry("40000", "b", r.sub.key),
        ]
        .concat(),
    );
    let objs = [top.clone(), r.sub.clone(), r.sub_blob.clone()];
    let mem = Mem::of(&objs);
    reachable_keys(top.key, |k| mem.get(k)).unwrap();
    assert_eq!(mem.reads(r.sub.key), 1);
    let mem = Mem::of(&objs);
    check_complete(top.key, |k| mem.get(k), |k| mem.has(k), 4).unwrap();
    assert_eq!(mem.reads(r.sub.key), 1);
}

/// File operations read directories and file content, and a git key is neither.
#[test]
fn file_operations_refuse_git_keys() {
    let r = GitRepo::new();
    let all = r.all();
    let mem = Mem::of(&all);
    for k in [r.tree.key, r.tip.key] {
        let err = dir_of(k, |k| mem.get(k)).unwrap_err();
        assert!(err.to_string().contains("GitSHA1"), "{err}");
    }
}

/// A store's Commit names the store's own objects. A git key may not stand in for
/// any of them (Go: `TestCommitRefusesGitKeys`).
#[test]
fn a_store_commit_refuses_git_keys() {
    use crate::commit::{Commit, Identity};
    let git = Key::new_git_sha1([1; crate::key::GIT_SHA1_SIZE]);
    let dir = encode_dir_leaf(&[]).unwrap().key;
    let id = Identity {
        name: "Ann".into(),
        when: 1,
        ..Default::default()
    };
    let base = Commit {
        tree: dir,
        parents: Vec::new(),
        author: id.clone(),
        committer: id,
        message: "m".into(),
        signature: Vec::new(),
        public_key: Vec::new(),
        change_id: Vec::new(),
        conflict_terms: Vec::new(),
        conflict_labels: Vec::new(),
    };
    let mut as_tree = base.clone();
    as_tree.tree = git;
    let mut as_parent = base.clone();
    as_parent.parents = vec![git];
    let mut as_term = base;
    as_term.conflict_terms = vec![git, git];
    for (name, c) in [
        ("as tree", as_tree),
        ("as parent", as_parent),
        ("as conflict term", as_term),
    ] {
        let err = c.object().err().unwrap_or_else(|| panic!("{name}: taken"));
        assert!(err.to_string().contains("GitSHA1"), "{name}: {err}");
    }
}
