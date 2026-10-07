//! Bottom-up tree builders: [`DirBuilder`] streams one directory's sorted
//! entries into DirLeaf runs and promotes their keys through a DirNode index;
//! [`IndexBuilder`] builds the index levels above any leaf stream (FileNode
//! levels over Blob keys, DirNode levels over DirLeaf keys). Objects are
//! emitted children-before-parents; the root is emitted last.
//!
//! The emit callback is the Rust shape of Go's `Emit func(Object) error`:
//! `FnMut(Object) -> Result<(), E>`, called once per built object. The
//! consumer (the pack driver) is responsible for writing the object to the
//! sink.

use std::fmt;

use super::encode::{marshal_entries, marshal_pairs};
use super::{DirPair, Entry, Error, Object, encode_dir_leaf, encode_dir_node, encode_file_node};
use crate::chunkers::ItemChunker;
use crate::key::Key;

/// Errors from the builders. Encoding failures and emit-callback failures are
/// propagated unwrapped (their [`fmt::Display`] is the inner error's), exactly
/// as the Go builders return them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError<E> {
    /// An object encoder failed (bad child key bytes or an invalid
    /// [`Entry::xattrs_in`] splice).
    Encode(Error),
    /// The caller's emit callback returned an error.
    Emit(E),
    /// `fstree: IndexBuilder.Finish with no children` — [`IndexBuilder::finish`]
    /// was called before any child was added.
    NoChildren,
}

impl<E: fmt::Display> fmt::Display for BuildError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Encode(e) => e.fmt(f),
            BuildError::Emit(e) => e.fmt(f),
            BuildError::NoChildren => f.write_str("fstree: IndexBuilder.Finish with no children"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for BuildError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BuildError::Encode(e) => Some(e),
            BuildError::Emit(e) => Some(e),
            BuildError::NoChildren => None,
        }
    }
}

/// Builds one directory's prolly tree by streaming its entries (which the
/// caller supplies already sorted bytewise by name). It chunks entries into
/// DirLeaf objects and promotes their keys through a DirNode index. Objects
/// are emitted children-before-parents; the directory root is emitted last.
pub struct DirBuilder {
    ic: ItemChunker,
    idx: IndexBuilder,
    leaf: Vec<Entry>,
    leaf_max: Vec<u8>,
    run_len: usize,
}

impl DirBuilder {
    /// Returns a `DirBuilder` using `ic` for both the entry stream and the
    /// DirNode index stream (Go `NewDirBuilder`).
    pub fn new(ic: ItemChunker) -> DirBuilder {
        DirBuilder {
            ic,
            idx: IndexBuilder::new_dir(ic),
            leaf: Vec::new(),
            leaf_max: Vec::new(),
            run_len: 0,
        }
    }

    /// Appends one directory entry (in sorted order).
    pub fn add_entry<F, E>(&mut self, emit: &mut F, e: Entry) -> Result<(), BuildError<E>>
    where
        F: FnMut(Object) -> Result<(), E>,
    {
        // Go marshals the entry before touching builder state, so a marshal
        // failure (an invalid xattrs_in splice) leaves the builder unchanged.
        // The canonical encoding of a one-entry array is the single array-head
        // byte 0x81 followed by exactly the entry's map encoding — the bytes
        // Go feeds to IsBoundary.
        let framed = marshal_entries(std::slice::from_ref(&e)).map_err(BuildError::Encode)?;
        let enc = &framed[1..];
        self.leaf_max = e.name.clone();
        self.leaf.push(e);
        self.run_len += 1;
        if self.ic.is_boundary(enc, self.run_len) {
            return self.close_leaf(emit);
        }
        Ok(())
    }

    /// Encodes and emits the open DirLeaf run, then promotes its key (with
    /// the run's greatest entry name as separator) into the DirNode index (Go
    /// `DirBuilder.closeLeaf`). On an encode failure the run is left in
    /// place, exactly as in Go.
    fn close_leaf<F, E>(&mut self, emit: &mut F) -> Result<(), BuildError<E>>
    where
        F: FnMut(Object) -> Result<(), E>,
    {
        let obj = encode_dir_leaf(&self.leaf).map_err(BuildError::Encode)?;
        let sep = std::mem::take(&mut self.leaf_max);
        self.leaf.clear();
        self.run_len = 0;
        let k = obj.key;
        emit(obj).map_err(BuildError::Emit)?;
        self.idx.add(emit, 0, k, &sep)
    }

    /// Closes the trailing leaf run (emitting an empty DirLeaf for an empty
    /// directory) and returns the directory's root key.
    pub fn finish<F, E>(mut self, emit: &mut F) -> Result<Key, BuildError<E>>
    where
        F: FnMut(Object) -> Result<(), E>,
    {
        if self.run_len > 0 || !self.idx.has_any() {
            self.close_leaf(emit)?;
        }
        self.idx.finish(emit)
    }
}

/// One index level's open run (Go `idxLevel`).
#[derive(Default)]
struct IdxLevel {
    keys: Vec<Key>,
    /// Populated only for directory indexes.
    seps: Vec<Vec<u8>>,
    run_len: usize,
}

/// Builds the index levels above a leaf level by streaming child references
/// and item-chunking them into FileNode (files) or DirNode (dirs) objects,
/// level by level, until one object remains (the root). Objects are emitted
/// children-before-parents; the root is emitted last. Memory is O(levels ×
/// MaxRun) — it never holds a whole level's bytes.
pub struct IndexBuilder {
    ic: ItemChunker,
    is_dir: bool,
    levels: Vec<IdxLevel>,
}

impl IndexBuilder {
    /// Builds FileNode levels over Blob/FileNode child keys (Go
    /// `NewFileIndexBuilder`).
    pub fn new_file(ic: ItemChunker) -> IndexBuilder {
        IndexBuilder {
            ic,
            is_dir: false,
            levels: Vec::new(),
        }
    }

    /// Builds DirNode levels over DirLeaf/DirNode child keys, each carrying
    /// the greatest entry name (sepName) in its subtree (Go
    /// `newDirIndexBuilder`; not exported there either — directories go
    /// through [`DirBuilder`]).
    fn new_dir(ic: ItemChunker) -> IndexBuilder {
        IndexBuilder {
            ic,
            is_dir: true,
            levels: Vec::new(),
        }
    }

    fn ensure(&mut self, l: usize) {
        while self.levels.len() <= l {
            self.levels.push(IdxLevel::default());
        }
    }

    /// Reports whether any child has been added (level 0 exists).
    fn has_any(&self) -> bool {
        !self.levels.is_empty()
    }

    /// Adds one leaf-level child reference. `sep` is the child subtree's
    /// greatest entry name for directory indexes, and is ignored (pass `&[]`)
    /// for file indexes.
    pub fn add_child<F, E>(
        &mut self,
        emit: &mut F,
        child_key: Key,
        sep: &[u8],
    ) -> Result<(), BuildError<E>>
    where
        F: FnMut(Object) -> Result<(), E>,
    {
        self.add(emit, 0, child_key, sep)
    }

    fn add<F, E>(
        &mut self,
        emit: &mut F,
        l: usize,
        ck: Key,
        sep: &[u8],
    ) -> Result<(), BuildError<E>>
    where
        F: FnMut(Object) -> Result<(), E>,
    {
        self.ensure(l);
        let level = &mut self.levels[l];
        level.keys.push(ck);
        if self.is_dir {
            level.seps.push(sep.to_vec());
        }
        level.run_len += 1;
        let run_len = level.run_len;
        // The item encoding fed to the boundary decision: the raw 32 key
        // bytes for file indexes, the canonical [sepName, childKey] pair
        // encoding for directory indexes (Go IndexBuilder.itemEnc; the
        // one-pair marshal is the array-head byte 0x81 plus that encoding).
        let boundary = if self.is_dir {
            let framed = marshal_pairs(&[DirPair {
                sep_name: sep.to_vec(),
                child_key: ck.as_bytes().to_vec(),
            }]);
            self.ic.is_boundary(&framed[1..], run_len)
        } else {
            self.ic.is_boundary(ck.as_bytes(), run_len)
        };
        if boundary {
            return self.close_level(emit, l);
        }
        Ok(())
    }

    fn close_level<F, E>(&mut self, emit: &mut F, l: usize) -> Result<(), BuildError<E>>
    where
        F: FnMut(Object) -> Result<(), E>,
    {
        let (obj, sep) = self.build_node(l)?;
        self.reset(l);
        let k = obj.key;
        emit(obj).map_err(BuildError::Emit)?;
        self.add(emit, l + 1, k, &sep)
    }

    fn reset(&mut self, l: usize) {
        let level = &mut self.levels[l];
        level.keys.clear();
        level.seps.clear();
        level.run_len = 0;
    }

    /// Builds the index object for level `l`'s current run, returning the
    /// object and the run's greatest sepName (empty for file indexes).
    fn build_node<E>(&self, l: usize) -> Result<(Object, Vec<u8>), BuildError<E>> {
        let level = &self.levels[l];
        if self.is_dir {
            let pairs: Vec<DirPair> = level
                .keys
                .iter()
                .zip(&level.seps)
                .map(|(k, s)| DirPair {
                    sep_name: s.clone(),
                    child_key: k.as_bytes().to_vec(),
                })
                .collect();
            let obj = encode_dir_node(&pairs).map_err(BuildError::Encode)?;
            // Invariant: build_node runs only with run_len >= 1, and dir
            // levels push one sep per key.
            let sep = level.seps.last().cloned().unwrap_or_default();
            Ok((obj, sep))
        } else {
            Ok((encode_file_node(&level.keys), Vec::new()))
        }
    }

    /// Collapses all open runs bottom-up and returns the root key. The single
    /// object that reaches the top with no parent is the root; a single child
    /// is returned without wrapping it in a degenerate one-child node.
    pub fn finish<F, E>(mut self, emit: &mut F) -> Result<Key, BuildError<E>>
    where
        F: FnMut(Object) -> Result<(), E>,
    {
        // `add` below can grow `levels`, so the bound is re-read every
        // iteration (Go: `for l := 0; l < len(ib.levels); l++`).
        let mut l = 0;
        while l < self.levels.len() {
            let is_top = l == self.levels.len() - 1;
            match self.levels[l].run_len {
                0 => {}
                1 => {
                    let ck = self.levels[l].keys[0];
                    let sep = if self.is_dir {
                        std::mem::take(&mut self.levels[l].seps[0])
                    } else {
                        Vec::new()
                    };
                    self.reset(l);
                    if is_top {
                        return Ok(ck); // already emitted when created
                    }
                    self.add(emit, l + 1, ck, &sep)?;
                }
                _ => {
                    let (obj, sep) = self.build_node(l)?;
                    self.reset(l);
                    let k = obj.key;
                    emit(obj).map_err(BuildError::Emit)?;
                    self.add(emit, l + 1, k, &sep)?;
                }
            }
            l += 1;
        }
        Err(BuildError::NoChildren)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;
    use crate::fstree::encode_blob;
    use crate::key::Type;

    /// Records emitted objects (Go test `collector`) and, like the Go oracle
    /// harness, folds `key ‖ bytes` of every emission into a BLAKE3 digest so
    /// the exact emit order and content can be compared against Go.
    #[derive(Default)]
    struct Collector {
        objs: Vec<Object>,
        digest: blake3::Hasher,
    }

    impl Collector {
        fn emit(&mut self) -> impl FnMut(Object) -> Result<(), Infallible> + '_ {
            |o| {
                self.digest.update(o.key.as_bytes());
                self.digest.update(&o.bytes);
                self.objs.push(o);
                Ok(())
            }
        }

        fn digest_hex(&self) -> String {
            self.digest
                .finalize()
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }
    }

    fn blob_key(n: usize) -> Key {
        encode_blob(&vec![0u8; n]).key
    }

    // --- ports of Go dir_builder_test.go ---

    #[test]
    fn dir_empty_dir_is_single_empty_leaf() {
        let mut c = Collector::default();
        let db = DirBuilder::new(ItemChunker::new(7));
        let root = db.finish(&mut c.emit()).unwrap();
        assert_eq!(root.type_(), Type::DirLeaf, "empty dir root type");
        assert_eq!(c.objs.len(), 1);
        assert_eq!(
            c.objs[0].key, root,
            "empty dir must emit exactly one DirLeaf that is the root"
        );
        // Empty DirLeaf is the CBOR empty array 0x80.
        assert_eq!(c.objs[0].bytes, vec![0x80], "empty DirLeaf bytes");
    }

    #[test]
    fn dir_single_entry_is_leaf_root() {
        let mut c = Collector::default();
        let mut db = DirBuilder::new(ItemChunker::new(7));
        db.add_entry(
            &mut c.emit(),
            Entry {
                name: b"a".to_vec(),
                mode: 0o100644,
                ..Default::default()
            },
        )
        .unwrap();
        let root = db.finish(&mut c.emit()).unwrap();
        assert_eq!(root.type_(), Type::DirLeaf, "root type");
        assert_eq!(
            c.objs.last().unwrap().key,
            root,
            "root must be the last emitted object"
        );
    }

    #[test]
    fn dir_many_entries_produce_dirnode_root_last() {
        let mut c = Collector::default();
        let mut db = DirBuilder::new(ItemChunker::new(2)); // tiny avg → multi-level
        for i in 0..5000 {
            let e = Entry {
                name: format!("{i:06}").into_bytes(), // already sorted
                mode: 0o100644,
                ..Default::default()
            };
            db.add_entry(&mut c.emit(), e).unwrap();
        }
        let root = db.finish(&mut c.emit()).unwrap();
        assert_eq!(root.type_(), Type::DirNode, "root type");
        assert_eq!(
            c.objs.last().unwrap().key,
            root,
            "root must be the last emitted object"
        );
        // du-style length must be positive and >= own bytes of the root.
        assert_ne!(root.length(), 0, "DirNode root length should be non-zero");

        // Differential oracle (Go at the pinned commit): the root key, the
        // emitted object count, and the BLAKE3 over every emission's
        // key ‖ bytes in emit order.
        assert_eq!(
            root.to_string(),
            "2eb3469c97cfd45b01f15676971bffee226a981bde1ab0dcc09aaaa8c54a0232"
        );
        assert_eq!(c.objs.len(), 1285, "emitted object count");
        assert_eq!(
            c.digest_hex(),
            "d787aaa22ca0d59e709d030b3e6daa07d4780174d420d08018cd8d812f766134"
        );
    }

    #[test]
    fn dir_three_entries_matches_go_oracle() {
        let mut c = Collector::default();
        let mut db = DirBuilder::new(ItemChunker::new(7));
        for n in [b"a", b"b", b"c"] {
            db.add_entry(
                &mut c.emit(),
                Entry {
                    name: n.to_vec(),
                    mode: 0o100644,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let root = db.finish(&mut c.emit()).unwrap();
        assert_eq!(
            root.to_string(),
            "aeb709736b0683cb13192a5232faa0165cc53d7171823191113fe9ddb4282b20"
        );
        assert_eq!(c.objs.len(), 1);
        assert_eq!(
            c.digest_hex(),
            "d4be5b7be50d62feae51e3a5edecd76988eb6263b65e6d28cf2e94a0b4fd52c0"
        );
    }

    // --- ports of Go index_builder_test.go ---

    #[test]
    fn file_index_single_child_is_root_no_node() {
        let mut c = Collector::default();
        let mut ib = IndexBuilder::new_file(ItemChunker::new(7));
        let k = blob_key(10);
        ib.add_child(&mut c.emit(), k, &[]).unwrap();
        let root = ib.finish(&mut c.emit()).unwrap();
        assert_eq!(root, k, "root should be the single blob");
        assert_eq!(
            c.objs.len(),
            0,
            "no FileNode should be emitted for a single child"
        );
    }

    #[test]
    fn file_index_multiple_children_produce_file_node_root() {
        let mut c = Collector::default();
        let mut ib = IndexBuilder::new_file(ItemChunker::new(7));
        let mut sum = 0u64;
        for n in [100, 200, 300] {
            let k = blob_key(n);
            sum += k.length();
            ib.add_child(&mut c.emit(), k, &[]).unwrap();
        }
        let root = ib.finish(&mut c.emit()).unwrap();
        assert_eq!(root.type_(), Type::FileNode, "root type");
        assert_eq!(root.length(), sum, "root length");
        // The root must be the LAST emitted object.
        assert_eq!(
            c.objs.last().expect("objects emitted").key,
            root,
            "root must be the last emitted object"
        );
    }

    #[test]
    fn file_index_many_children_multi_level() {
        let mut c = Collector::default();
        let mut ib = IndexBuilder::new_file(ItemChunker::new(2)); // tiny avg → multiple levels
        for i in 0..2000 {
            ib.add_child(&mut c.emit(), blob_key(i + 1), &[]).unwrap();
        }
        let root = ib.finish(&mut c.emit()).unwrap();
        assert_eq!(root.type_(), Type::FileNode, "root type");
        assert_eq!(
            c.objs.last().unwrap().key,
            root,
            "root must be the last emitted object"
        );

        // Differential oracle (Go at the pinned commit).
        assert_eq!(
            root.to_string(),
            "2868a1999f0876f338b1ff9ecb8ebc918ac3256ff049c7ed9d33973768881e12"
        );
        assert_eq!(root.length(), 2_001_000, "Σ blob sizes 1..=2000");
        assert_eq!(c.objs.len(), 484, "emitted object count");
        assert_eq!(
            c.digest_hex(),
            "85bc39bd1b5482c0583a80a084b117d162b005873b72b25bbad42a6dee2f5a2d"
        );
    }

    #[test]
    fn file_index_finish_with_no_children_errors() {
        let mut c = Collector::default();
        let ib = IndexBuilder::new_file(ItemChunker::new(7));
        let err = ib.finish(&mut c.emit()).unwrap_err();
        assert!(matches!(err, BuildError::NoChildren));
        assert_eq!(
            err.to_string(),
            "fstree: IndexBuilder.Finish with no children"
        );
    }

    #[test]
    fn emit_error_propagates() {
        let mut db = DirBuilder::new(ItemChunker::new(7));
        db.add_entry(
            &mut |_: Object| Ok::<(), &str>(()),
            Entry {
                name: b"a".to_vec(),
                ..Default::default()
            },
        )
        .unwrap();
        let err = db.finish(&mut |_| Err("sink full")).unwrap_err();
        assert!(matches!(err, BuildError::Emit("sink full")));
        assert_eq!(err.to_string(), "sink full");
    }

    #[test]
    fn add_entry_encode_error_leaves_builder_usable() {
        // An invalid xattrs_in splice fails the marshal before any state
        // change; the builder still finishes the valid entries.
        let mut c = Collector::default();
        let mut db = DirBuilder::new(ItemChunker::new(7));
        db.add_entry(
            &mut c.emit(),
            Entry {
                name: b"a".to_vec(),
                ..Default::default()
            },
        )
        .unwrap();
        let err = db
            .add_entry(
                &mut c.emit(),
                Entry {
                    name: b"bad".to_vec(),
                    xattrs_in: vec![0xff],
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, BuildError::Encode(_)));
        let root = db.finish(&mut c.emit()).unwrap();
        assert_eq!(root.type_(), Type::DirLeaf);
        let entries = crate::fstree::decode_dir_leaf(&c.objs.last().unwrap().bytes).unwrap();
        assert_eq!(entries.len(), 1, "failed entry must not be retained");
        assert_eq!(entries[0].name, b"a");
    }
}
