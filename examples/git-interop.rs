//! Probe for interop/check.sh: the same small history as interop/git.go, written
//! and read by the other implementation. Both must give it the same keys.
use amber_store_core::fstree;
use amber_store_core::gitobj::{self, Kind};
use amber_store_core::key::Key;
use amber_store_core::packstore::{Options, Store};

struct Obj {
    key: Key,
    data: Vec<u8>,
}

fn obj(kind: Kind, body: &[u8]) -> Obj {
    let data = gitobj::encode(kind, body);
    Obj {
        key: gitobj::key_of(&data).unwrap(),
        data,
    }
}

fn entry(mode: &str, name: &str, o: &Obj) -> Vec<u8> {
    let mut b = format!("{mode} {name}\0").into_bytes();
    b.extend_from_slice(&o.key.git_sha1_sum().unwrap());
    b
}

fn oid(o: &Obj) -> String {
    o.key
        .git_sha1_sum()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the root reaches, the root last, and what it does not.
fn history() -> (Vec<Obj>, Obj) {
    let blob = obj(Kind::Blob, b"hello\n");
    let sub = obj(Kind::Tree, &entry("100644", "f", &blob));
    let tree = obj(
        Kind::Tree,
        &[
            entry("100755", "run", &blob),
            entry("120000", "sym", &blob),
            entry("160000", "mod", &blob),
            entry("40000", "z", &sub),
        ]
        .concat(),
    );
    let first = obj(
        Kind::Commit,
        format!(
            "tree {}\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\n\nfirst\n",
            oid(&sub)
        )
        .as_bytes(),
    );
    let tip = obj(
        Kind::Commit,
        format!(
            "tree {}\nparent {}\nauthor a <a@b> 2 +0000\ncommitter a <a@b> 2 +0000\n\nsecond\n",
            oid(&tree),
            oid(&first)
        )
        .as_bytes(),
    );
    let tag = obj(
        Kind::Tag,
        format!("object {}\ntype commit\ntag v1\n\nm\n", oid(&tip)).as_bytes(),
    );
    (
        vec![blob, sub, tree, first, tip, tag],
        obj(Kind::Blob, b"reached by nothing\n"),
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let (reached, dangling) = history();
    let root = reached.last().unwrap().key;
    let opts = || Options::default().segment_size(256);
    match args[1].as_str() {
        "keys" => println!("{root} {} {}", reached[0].key, dangling.key),
        "create" => {
            let store = Store::open_with(&args[2], opts())?;
            for o in reached.iter().chain([&dangling]) {
                store.put_verified(o.key, &o.data)?;
            }
            store.close()?;
        }
        "check" => {
            let store = Store::open_with(&args[2], opts())?;
            for o in &reached {
                assert_eq!(store.get(o.key)?, o.data, "content mismatch");
            }
            let mut visited = fstree::check_complete(root, |k| store.get(k), |k| store.has(k), 4)?;
            let mut want: Vec<Key> = reached.iter().map(|o| o.key).collect();
            visited.sort();
            want.sort();
            assert_eq!(
                visited, want,
                "the walk does not reach what the history holds"
            );
            store.verify(|| false)?;
            store.close()?;
        }
        "gone" => {
            let store = Store::open_with(&args[2], opts())?;
            assert!(
                !store.has(dangling.key)?,
                "the object nothing reaches outlived the collection"
            );
            store.close()?;
        }
        _ => panic!("expected keys, create, check, or gone"),
    }
    Ok(())
}
