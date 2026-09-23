//! Read paths over stored tree objects: child enumeration, path resolution,
//! entry lookup and listing, content streaming, reachability, and
//! completeness checking.
//!
//! The getter is the Rust shape of Go's `func(key.Key) ([]byte, error)`:
//! `FnMut(Key) -> Result<Vec<u8>, E>` for the sequential paths, and
//! `Fn(Key) -> Result<Vec<u8>, E> + Sync` for [`reachable_keys`] and
//! [`check_complete`], whose Go counterparts require a getter that is safe
//! for concurrent use.

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Entry, Error, decode_dir_leaf, decode_dir_node, decode_file_node};
use crate::key::{self, Key, Type};

/// POSIX file-type mask/dir bits (Go uses `unix.S_IFMT` / `unix.S_IFDIR`;
/// the values are universal).
const S_IFMT: u64 = 0o170000;
const S_IFDIR: u64 = 0o040000;

/// Errors from [`child_keys`], one variant per Go wrap site with the same
/// diagnostic text.
///
/// Go's trailing `fstree: unknown object type %s` branch is unrepresentable:
/// [`Type`] admits only the six defined types, all of which are handled, and
/// a non-canonical key panics in [`Key::type_`] per that method's contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildKeysError {
    /// `fstree: decoding FileNode <key>: <err>`
    DecodeFileNode {
        /// The FileNode object's key.
        key: Key,
        /// The decode failure.
        source: Error,
    },
    /// `fstree: decoding DirNode <key>: <err>`
    DecodeDirNode {
        /// The DirNode object's key.
        key: Key,
        /// The decode failure.
        source: Error,
    },
    /// `fstree: child key in DirNode <key>: <err>`
    DirNodeChildKey {
        /// The DirNode object's key.
        key: Key,
        /// The child-key parse failure.
        source: key::Error,
    },
    /// `fstree: decoding DirLeaf <key>: <err>`
    DecodeDirLeaf {
        /// The DirLeaf object's key.
        key: Key,
        /// The decode failure.
        source: Error,
    },
    /// `fstree: decoding Commit <key>: <err>`
    DecodeCommit {
        /// The Commit object's key.
        key: Key,
        /// The decode failure.
        source: crate::commit::Error,
    },
    /// `fstree: "<name>": content key: <err>`
    EntryContentKey {
        /// The entry's name (rendered with `%q` semantics).
        name: Vec<u8>,
        /// The content-key parse failure.
        source: key::Error,
    },
    /// `fstree: "<name>": xattrs key: <err>`
    EntryXattrsKey {
        /// The entry's name (rendered with `%q` semantics).
        name: Vec<u8>,
        /// The xattrs-key parse failure.
        source: key::Error,
    },
}

impl fmt::Display for ChildKeysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChildKeysError::DecodeFileNode { key, source } => {
                write!(f, "fstree: decoding FileNode {key}: {source}")
            }
            ChildKeysError::DecodeDirNode { key, source } => {
                write!(f, "fstree: decoding DirNode {key}: {source}")
            }
            ChildKeysError::DirNodeChildKey { key, source } => {
                write!(f, "fstree: child key in DirNode {key}: {source}")
            }
            ChildKeysError::DecodeDirLeaf { key, source } => {
                write!(f, "fstree: decoding DirLeaf {key}: {source}")
            }
            ChildKeysError::DecodeCommit { key, source } => {
                write!(f, "fstree: decoding Commit {key}: {source}")
            }
            ChildKeysError::EntryContentKey { name, source } => {
                write!(
                    f,
                    "fstree: {:?}: content key: {source}",
                    String::from_utf8_lossy(name)
                )
            }
            ChildKeysError::EntryXattrsKey { name, source } => {
                write!(
                    f,
                    "fstree: {:?}: xattrs key: {source}",
                    String::from_utf8_lossy(name)
                )
            }
        }
    }
}

impl std::error::Error for ChildKeysError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ChildKeysError::DecodeFileNode { source, .. }
            | ChildKeysError::DecodeDirNode { source, .. }
            | ChildKeysError::DecodeDirLeaf { source, .. } => Some(source),
            ChildKeysError::DecodeCommit { source, .. } => Some(source),
            ChildKeysError::DirNodeChildKey { source, .. }
            | ChildKeysError::EntryContentKey { source, .. }
            | ChildKeysError::EntryXattrsKey { source, .. } => Some(source),
        }
    }
}

/// Reports a leaf object found absent by [`check_complete`] (Go
/// `*MissingObjectError`): `fstree: object <key> is missing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingObjectError {
    /// The absent object's key.
    pub key: Key,
}

impl fmt::Display for MissingObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fstree: object {} is missing", self.key)
    }
}

impl std::error::Error for MissingObjectError {}

/// Errors from the tree read paths, one variant per Go wrap site with the
/// same diagnostic text. `E` is the caller's getter/has error type.
///
/// Variants that Go returns unwrapped ([`WalkError::Children`],
/// [`WalkError::Missing`], [`WalkError::Has`], [`WalkError::Codec`],
/// [`WalkError::Io`]) display transparently as the inner error. Where Go code
/// would use `errors.Is(err, ErrNotFound)` / `errors.Is(err, ErrNotDir)`,
/// match the variant or use [`WalkError::is_not_found`] /
/// [`WalkError::is_not_dir`].
#[derive(Debug)]
pub enum WalkError<E> {
    /// `fstree: reading <key>: <err>` — the getter failed.
    Read {
        /// The key being fetched.
        key: Key,
        /// The getter's error.
        source: E,
    },
    /// `fstree: decoding DirLeaf <key>: <err>`
    DecodeDirLeaf {
        /// The DirLeaf object's key.
        key: Key,
        /// The decode failure.
        source: Error,
    },
    /// `fstree: decoding DirNode <key>: <err>`
    DecodeDirNode {
        /// The DirNode object's key.
        key: Key,
        /// The decode failure.
        source: Error,
    },
    /// `fstree: child key in DirNode <key>: <err>`
    ChildKey {
        /// The DirNode object's key.
        key: Key,
        /// The child-key parse failure.
        source: key::Error,
    },
    /// `fstree: DirLeaf <key>: entry "<entry>" is not after "<prev>"` —
    /// entry names failed to strictly increase across the collected leaves.
    OutOfOrder {
        /// The DirLeaf object's key.
        key: Key,
        /// The offending entry's name.
        entry: Vec<u8>,
        /// The previously collected name it failed to sort after.
        prev: Vec<u8>,
    },
    /// `fstree: <key> is not a directory object (type <type>)`
    NotDirObject {
        /// The non-directory object's key.
        key: Key,
    },
    /// `fstree: "<name>": entry not found` (Go `ErrNotFound`).
    NotFound {
        /// The name that was looked up.
        name: Vec<u8>,
    },
    /// `fstree: "<name>": not a directory` (Go `ErrNotDir`).
    NotDir {
        /// The path component that is not a directory.
        name: Vec<u8>,
    },
    /// `fstree: "<path>": ".." is not supported`
    DotDot {
        /// The full path containing the rejected component.
        path: String,
    },
    /// `fstree: "<name>": content key: <err>` (path resolution).
    ContentKey {
        /// The directory entry's name.
        name: Vec<u8>,
        /// The content-key parse failure.
        source: key::Error,
    },
    /// `fstree: ListEntries limit must be positive, got <limit>`
    BadLimit {
        /// The rejected limit.
        limit: usize,
    },
    /// A [`child_keys`] failure, returned unwrapped as in Go.
    Children(ChildKeysError),
    /// A leaf object is absent ([`check_complete`]), returned unwrapped.
    Missing(MissingObjectError),
    /// The `has` callback failed ([`check_complete`]), returned unwrapped.
    Has(E),
    /// `reading <key>: <err>` — [`write_content`]'s getter failure (Go's
    /// message here carries no `fstree:` prefix).
    ContentRead {
        /// The key being fetched.
        key: Key,
        /// The getter's error.
        source: E,
    },
    /// `<key> is not a file-content object (type <type>)` (no `fstree:`
    /// prefix, as in Go).
    NotContentObject {
        /// The non-content object's key.
        key: Key,
    },
    /// A FileNode decode failure in [`write_content`], returned unwrapped.
    Codec(Error),
    /// A writer failure in [`write_content`], returned unwrapped.
    Io(io::Error),
}

impl<E> WalkError<E> {
    /// Reports whether this is the missing-name error (Go
    /// `errors.Is(err, ErrNotFound)`).
    pub fn is_not_found(&self) -> bool {
        matches!(self, WalkError::NotFound { .. })
    }

    /// Reports whether this is the not-a-directory error (Go
    /// `errors.Is(err, ErrNotDir)`).
    pub fn is_not_dir(&self) -> bool {
        matches!(self, WalkError::NotDir { .. })
    }

    /// Returns the [`MissingObjectError`] if this error is one (Go
    /// `errors.As(err, &missing)`).
    pub fn missing_object(&self) -> Option<&MissingObjectError> {
        match self {
            WalkError::Missing(m) => Some(m),
            _ => None,
        }
    }
}

impl<E: fmt::Display> fmt::Display for WalkError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::Read { key, source } => write!(f, "fstree: reading {key}: {source}"),
            WalkError::DecodeDirLeaf { key, source } => {
                write!(f, "fstree: decoding DirLeaf {key}: {source}")
            }
            WalkError::DecodeDirNode { key, source } => {
                write!(f, "fstree: decoding DirNode {key}: {source}")
            }
            WalkError::ChildKey { key, source } => {
                write!(f, "fstree: child key in DirNode {key}: {source}")
            }
            WalkError::OutOfOrder { key, entry, prev } => {
                write!(
                    f,
                    "fstree: DirLeaf {key}: entry {:?} is not after {:?}",
                    String::from_utf8_lossy(entry),
                    String::from_utf8_lossy(prev)
                )
            }
            WalkError::NotDirObject { key } => {
                write!(
                    f,
                    "fstree: {key} is not a directory object (type {})",
                    key.type_()
                )
            }
            WalkError::NotFound { name } => {
                write!(
                    f,
                    "fstree: {:?}: entry not found",
                    String::from_utf8_lossy(name)
                )
            }
            WalkError::NotDir { name } => {
                write!(
                    f,
                    "fstree: {:?}: not a directory",
                    String::from_utf8_lossy(name)
                )
            }
            WalkError::DotDot { path } => write!(f, "fstree: {path:?}: \"..\" is not supported"),
            WalkError::ContentKey { name, source } => {
                write!(
                    f,
                    "fstree: {:?}: content key: {source}",
                    String::from_utf8_lossy(name)
                )
            }
            WalkError::BadLimit { limit } => {
                write!(f, "fstree: ListEntries limit must be positive, got {limit}")
            }
            WalkError::Children(e) => e.fmt(f),
            WalkError::Missing(e) => e.fmt(f),
            WalkError::Has(e) => e.fmt(f),
            WalkError::ContentRead { key, source } => write!(f, "reading {key}: {source}"),
            WalkError::NotContentObject { key } => {
                write!(
                    f,
                    "{key} is not a file-content object (type {})",
                    key.type_()
                )
            }
            WalkError::Codec(e) => e.fmt(f),
            WalkError::Io(e) => e.fmt(f),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for WalkError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WalkError::Read { source, .. } | WalkError::ContentRead { source, .. } => Some(source),
            WalkError::Has(source) => Some(source),
            WalkError::DecodeDirLeaf { source, .. } | WalkError::DecodeDirNode { source, .. } => {
                Some(source)
            }
            WalkError::Codec(source) => Some(source),
            WalkError::ChildKey { source, .. } | WalkError::ContentKey { source, .. } => {
                Some(source)
            }
            WalkError::Children(e) => Some(e),
            WalkError::Missing(e) => Some(e),
            WalkError::Io(e) => Some(e),
            WalkError::NotDirObject { .. }
            | WalkError::OutOfOrder { .. }
            | WalkError::NotFound { .. }
            | WalkError::NotDir { .. }
            | WalkError::DotDot { .. }
            | WalkError::BadLimit { .. }
            | WalkError::NotContentObject { .. } => None,
        }
    }
}

/// Returns the keys directly referenced by the object with key `k` and
/// serialized bytes `data`, in encounter order. Blob and XattrSet objects are
/// leaves and have no children. A Commit's children are its tree, then its
/// parents in recorded order — so every walk built on `child_keys` follows
/// history.
pub fn child_keys(k: Key, data: &[u8]) -> Result<Vec<Key>, ChildKeysError> {
    match k.type_() {
        Type::Blob | Type::XattrSet => Ok(Vec::new()),
        Type::FileNode => decode_file_node(data)
            .map_err(|source| ChildKeysError::DecodeFileNode { key: k, source }),
        Type::DirNode => {
            let pairs = decode_dir_node(data)
                .map_err(|source| ChildKeysError::DecodeDirNode { key: k, source })?;
            let mut out = Vec::with_capacity(pairs.len());
            for p in &pairs {
                let ck = Key::parse(&p.child_key)
                    .map_err(|source| ChildKeysError::DirNodeChildKey { key: k, source })?;
                out.push(ck);
            }
            Ok(out)
        }
        Type::DirLeaf => {
            let entries = decode_dir_leaf(data)
                .map_err(|source| ChildKeysError::DecodeDirLeaf { key: k, source })?;
            let mut out = Vec::new();
            for ent in &entries {
                if !ent.content_key.is_empty() {
                    let ck = Key::parse(&ent.content_key).map_err(|source| {
                        ChildKeysError::EntryContentKey {
                            name: ent.name.clone(),
                            source,
                        }
                    })?;
                    out.push(ck);
                }
                if !ent.xattrs_key.is_empty() {
                    let xk = Key::parse(&ent.xattrs_key).map_err(|source| {
                        ChildKeysError::EntryXattrsKey {
                            name: ent.name.clone(),
                            source,
                        }
                    })?;
                    out.push(xk);
                }
            }
            Ok(out)
        }
        Type::Commit => {
            let c = crate::commit::Commit::decode(data)
                .map_err(|source| ChildKeysError::DecodeCommit { key: k, source })?;
            let mut out = Vec::with_capacity(1 + c.parents.len());
            out.push(c.tree);
            out.extend(c.parents);
            Ok(out)
        }
    }
}

/// Returns the entry called `name` in the directory object `dir`. It descends
/// DirNode levels by binary search over each pair's sepName (the greatest
/// entry name in that child's subtree), then scans the one DirLeaf that could
/// hold the name — O(log n) objects for an n-entry directory. A missing name
/// is [`WalkError::NotFound`]; `get` fetches the bytes stored under a key.
pub fn lookup_entry<G, E>(dir: Key, name: &[u8], mut get: G) -> Result<Entry, WalkError<E>>
where
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    let mut k = dir;
    loop {
        let data = get(k).map_err(|source| WalkError::Read { key: k, source })?;
        match k.type_() {
            Type::DirLeaf => {
                let mut entries = decode_dir_leaf(&data)
                    .map_err(|source| WalkError::DecodeDirLeaf { key: k, source })?;
                let i = entries.partition_point(|e| e.name.as_slice() < name);
                if i < entries.len() && entries[i].name == name {
                    return Ok(entries.swap_remove(i));
                }
                return Err(WalkError::NotFound {
                    name: name.to_vec(),
                });
            }
            Type::DirNode => {
                let pairs = decode_dir_node(&data)
                    .map_err(|source| WalkError::DecodeDirNode { key: k, source })?;
                // The first pair whose sepName >= name roots the only subtree
                // that can contain name.
                let i = pairs.partition_point(|p| p.sep_name.as_slice() < name);
                if i == pairs.len() {
                    return Err(WalkError::NotFound {
                        name: name.to_vec(),
                    });
                }
                let ck = Key::parse(&pairs[i].child_key)
                    .map_err(|source| WalkError::ChildKey { key: k, source })?;
                k = ck;
            }
            _ => return Err(WalkError::NotDirObject { key: k }),
        }
    }
}

/// Descends from the directory object `root` along the slash-separated path
/// and returns the key of the directory it names. Empty components and `"."`
/// are ignored, so `""`, `"."`, and paths with leading/trailing slashes are
/// accepted; `".."` is rejected (a CAS tree has no parent links). A missing
/// component is [`WalkError::NotFound`], a non-directory component
/// [`WalkError::NotDir`].
pub fn resolve_path<G, E>(root: Key, path: &str, mut get: G) -> Result<Key, WalkError<E>>
where
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    let mut k = root;
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err(WalkError::DotDot {
                path: path.to_string(),
            });
        }
        let found = lookup_entry(k, comp.as_bytes(), &mut get)?;
        if found.mode & S_IFMT != S_IFDIR {
            return Err(WalkError::NotDir {
                name: comp.as_bytes().to_vec(),
            });
        }
        k = Key::parse(&found.content_key).map_err(|source| WalkError::ContentKey {
            name: comp.as_bytes().to_vec(),
            source,
        })?;
    }
    Ok(k)
}

/// Descends from the directory object `root` along the slash-separated path
/// and returns the entry the final component names — of any kind (file,
/// directory, symlink, device, …), carrying its metadata. The empty path (or
/// chains of `""` and `"."`) returns `None`: the root directory is not an
/// entry and has no metadata of its own. Intermediate components must name
/// directories ([`WalkError::NotDir`] otherwise); a missing component is
/// [`WalkError::NotFound`]; `".."` is rejected.
pub fn resolve_entry<G, E>(root: Key, path: &str, mut get: G) -> Result<Option<Entry>, WalkError<E>>
where
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    let mut dir = root;
    let mut cur: Option<Entry> = None; // entry of dir; None while dir is the root
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err(WalkError::DotDot {
                path: path.to_string(),
            });
        }
        if let Some(c) = &cur {
            if c.mode & S_IFMT != S_IFDIR {
                return Err(WalkError::NotDir {
                    name: c.name.clone(),
                });
            }
            dir = Key::parse(&c.content_key).map_err(|source| WalkError::ContentKey {
                name: c.name.clone(),
                source,
            })?;
        }
        cur = Some(lookup_entry(dir, comp.as_bytes(), &mut get)?);
    }
    Ok(cur)
}

/// Returns the directory entries reachable from `k`, descending DirNode index
/// levels into the DirLeaves that hold them. Entries are returned in name
/// order (the order the leaves store them). `get` fetches the bytes stored
/// under a key.
///
/// Names must strictly increase across leaves. This also stops a pushed DAG
/// whose DirNodes repeat one child from expanding multiplicatively.
pub fn collect_entries<G, E>(k: Key, mut get: G) -> Result<Vec<Entry>, WalkError<E>>
where
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    let mut out = Vec::new();
    collect_inner(k, &mut get, &mut out)?;
    Ok(out)
}

fn collect_inner<G, E>(k: Key, get: &mut G, out: &mut Vec<Entry>) -> Result<(), WalkError<E>>
where
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    let data = get(k).map_err(|source| WalkError::Read { key: k, source })?;
    match k.type_() {
        Type::DirLeaf => {
            let entries = decode_dir_leaf(&data)
                .map_err(|source| WalkError::DecodeDirLeaf { key: k, source })?;
            for e in entries {
                if let Some(prev) = out.last()
                    && e.name <= prev.name
                {
                    return Err(WalkError::OutOfOrder {
                        key: k,
                        entry: e.name.clone(),
                        prev: prev.name.clone(),
                    });
                }
                out.push(e);
            }
            Ok(())
        }
        Type::DirNode => {
            let pairs = decode_dir_node(&data)
                .map_err(|source| WalkError::DecodeDirNode { key: k, source })?;
            for p in &pairs {
                let ck = Key::parse(&p.child_key)
                    .map_err(|source| WalkError::ChildKey { key: k, source })?;
                collect_inner(ck, get, out)?;
            }
            Ok(())
        }
        _ => Err(WalkError::NotDirObject { key: k }),
    }
}

/// Returns up to `limit` entries of the directory object `dir` whose names
/// sort strictly after `after` (empty lists from the start), in name order,
/// and whether more such entries follow. When more is true the returned page
/// is full: `entries.len() == limit`. It descends only the subtrees that can
/// hold qualifying names — a DirNode pair is skipped when its sepName (the
/// subtree's greatest name) is not after `after` — touching O(log n + limit)
/// objects. `limit` must be positive.
pub fn list_entries<G, E>(
    dir: Key,
    after: &[u8],
    limit: usize,
    mut get: G,
) -> Result<(Vec<Entry>, bool), WalkError<E>>
where
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    if limit == 0 {
        return Err(WalkError::BadLimit { limit });
    }
    let mut out = Vec::new();
    let more = list_into(&mut out, dir, after, limit, &mut get)?;
    Ok((out, more))
}

/// Appends qualifying entries of the subtree at `k` to `out` until `out`
/// holds `limit` entries; reports true the moment a qualifying entry beyond
/// the limit exists.
fn list_into<G, E>(
    out: &mut Vec<Entry>,
    k: Key,
    after: &[u8],
    limit: usize,
    get: &mut G,
) -> Result<bool, WalkError<E>>
where
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    let data = get(k).map_err(|source| WalkError::Read { key: k, source })?;
    match k.type_() {
        Type::DirLeaf => {
            let entries = decode_dir_leaf(&data)
                .map_err(|source| WalkError::DecodeDirLeaf { key: k, source })?;
            let i = entries.partition_point(|e| e.name.as_slice() <= after);
            for e in entries.into_iter().skip(i) {
                if out.len() == limit {
                    return Ok(true);
                }
                out.push(e);
            }
            Ok(false)
        }
        Type::DirNode => {
            let pairs = decode_dir_node(&data)
                .map_err(|source| WalkError::DecodeDirNode { key: k, source })?;
            let i = pairs.partition_point(|p| p.sep_name.as_slice() <= after);
            for p in &pairs[i..] {
                let ck = Key::parse(&p.child_key)
                    .map_err(|source| WalkError::ChildKey { key: k, source })?;
                if list_into(out, ck, after, limit, get)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Err(WalkError::NotDirObject { key: k }),
    }
}

/// Writes the regular-file content addressed by `k` to `w`, descending
/// FileNode index levels and concatenating Blob leaves in order. `k` must be
/// a Blob or FileNode object.
pub fn write_content<W, G, E>(w: &mut W, k: Key, mut get: G) -> Result<(), WalkError<E>>
where
    W: io::Write + ?Sized,
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    content_inner(w, k, &mut get)
}

fn content_inner<W, G, E>(w: &mut W, k: Key, get: &mut G) -> Result<(), WalkError<E>>
where
    W: io::Write + ?Sized,
    G: FnMut(Key) -> Result<Vec<u8>, E>,
{
    let data = get(k).map_err(|source| WalkError::ContentRead { key: k, source })?;
    match k.type_() {
        Type::Blob => w.write_all(&data).map_err(WalkError::Io),
        Type::FileNode => {
            let children = decode_file_node(&data).map_err(WalkError::Codec)?;
            for ck in children {
                content_inner(w, ck, get)?;
            }
            Ok(())
        }
        _ => Err(WalkError::NotContentObject { key: k }),
    }
}

/// Returns the keys of every object reachable from `root` — the set that must
/// be transferred to hold the whole content — with `root` first and the
/// remaining keys in unspecified order (callers must not rely on any ordering
/// beyond root-first), each key listed once even when referenced repeatedly.
/// `root` may be any object type. `get` fetches the bytes stored under a key;
/// Blob and XattrSet objects are leaves and are not fetched.
///
/// The walk is a breadth-first sweep that fetches each round's frontier
/// concurrently (bounded by the available parallelism), so a wide tree of
/// small objects is read in parallel. The current implementation happens to
/// be deterministic — results are indexed by frontier position, matching the
/// Go implementation's order — but that is not part of the contract. `get`
/// must be safe for concurrent use (`Fn + Sync`, the Rust spelling of the Go
/// requirement).
pub fn reachable_keys<G, E>(root: Key, get: G) -> Result<Vec<Key>, WalkError<E>>
where
    G: Fn(Key) -> Result<Vec<u8>, E> + Sync,
    E: Send,
{
    let jobs = default_jobs();
    let mut seen: HashSet<Key> = HashSet::from([root]);
    let mut out = vec![root];
    let mut frontier = vec![root];

    while !frontier.is_empty() {
        // Fetch each frontier node's children concurrently; indexing by
        // position keeps the result deterministic regardless of completion
        // order.
        let child_lists = parallel_map(&frontier, jobs, |k| {
            if matches!(k.type_(), Type::Blob | Type::XattrSet) {
                return Ok(Vec::new()); // leaf: no children, nothing to fetch
            }
            let data = get(k).map_err(|source| WalkError::Read { key: k, source })?;
            child_keys(k, &data).map_err(WalkError::Children)
        });

        let mut next = Vec::new();
        for children in child_lists {
            for ck in children? {
                if seen.insert(ck) {
                    out.push(ck);
                    next.push(ck);
                }
            }
        }
        frontier = next;
    }
    Ok(out)
}

/// Verifies that every object reachable from `root` exists and returns the
/// visited keys — root first, then discovery order (per BFS level, in each
/// parent's child order), each key exactly once even when referenced
/// repeatedly (Go: `CheckComplete`). The tree is walked breadth-first,
/// checking each level's objects with up to `jobs` concurrent lookups
/// (`jobs == 0` means the available parallelism, like Go's `jobs <= 0` ⇒
/// GOMAXPROCS): interior nodes are read with `get` — a failed read surfaces
/// as [`WalkError::Read`] wrapping the get error — and Blob and XattrSet
/// leaves are tested with `has` — an absent leaf surfaces as
/// [`WalkError::Missing`]. On error there is no partial list — `Err` is
/// returned (Go returns a nil visited list). `get` and `has` must be safe
/// for concurrent use.
pub fn check_complete<G, H, E>(
    root: Key,
    get: G,
    has: H,
    jobs: usize,
) -> Result<Vec<Key>, WalkError<E>>
where
    G: Fn(Key) -> Result<Vec<u8>, E> + Sync,
    H: Fn(Key) -> Result<bool, E> + Sync,
    E: Send,
{
    let jobs = if jobs == 0 { default_jobs() } else { jobs };
    let mut visited = vec![root];
    let mut seen: HashSet<Key> = HashSet::from([root]);
    let mut frontier = vec![root];

    while !frontier.is_empty() {
        let results = parallel_map(&frontier, jobs, |k| {
            if matches!(k.type_(), Type::Blob | Type::XattrSet) {
                return match has(k) {
                    Ok(true) => Ok(Vec::new()),
                    Ok(false) => Err(WalkError::Missing(MissingObjectError { key: k })),
                    Err(e) => Err(WalkError::Has(e)),
                };
            }
            let data = get(k).map_err(|source| WalkError::Read { key: k, source })?;
            child_keys(k, &data).map_err(WalkError::Children)
        });

        let mut next = Vec::new();
        for children in results {
            for ck in children? {
                if seen.insert(ck) {
                    visited.push(ck);
                    next.push(ck);
                }
            }
        }
        frontier = next;
    }
    Ok(visited)
}

/// The default worker count for the parallel walks (Go: `GOMAXPROCS`).
fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
}

/// Applies `f` to every item with up to `jobs` scoped worker threads and
/// returns the per-index results — deterministic regardless of scheduling.
/// All items are processed even if some fail (as Go's errgroup runs every
/// submitted task); the caller picks the first error in index order.
fn parallel_map<T, E, F>(items: &[Key], jobs: usize, f: F) -> Vec<Result<T, E>>
where
    F: Fn(Key) -> Result<T, E> + Sync,
    T: Send,
    E: Send,
{
    let workers = jobs.min(items.len());
    if workers <= 1 {
        return items.iter().map(|&k| f(k)).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<T, E>>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= items.len() {
                        break;
                    }
                    let r = f(items[i]);
                    *slots[i].lock().expect("result slot lock") = Some(r);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|m| {
            m.into_inner()
                .expect("result slot lock")
                .expect("worker filled every slot")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;

    use super::super::{
        DirBuilder, DirPair, Object, encode_blob, encode_dir_leaf, encode_dir_node,
        encode_file_node, encode_xattr_set,
    };
    use super::*;
    use crate::chunkers::ItemChunker;

    /// An in-memory object store for builder-emitted objects (Go `memStore` /
    /// `mapGetter`).
    #[derive(Default, Clone)]
    struct MemStore(HashMap<Key, Vec<u8>>);

    impl MemStore {
        fn of(objs: &[&Object]) -> MemStore {
            let mut m = MemStore::default();
            for o in objs {
                m.insert(o);
            }
            m
        }

        fn insert(&mut self, o: &Object) {
            self.0.insert(o.key, o.bytes.clone());
        }

        fn get(&self) -> impl Fn(Key) -> Result<Vec<u8>, String> + Sync + '_ {
            |k| {
                self.0
                    .get(&k)
                    .cloned()
                    .ok_or_else(|| format!("object {k} not in store"))
            }
        }

        fn has(&self) -> impl Fn(Key) -> Result<bool, String> + Sync + '_ {
            |k| Ok(self.0.contains_key(&k))
        }

        fn emit(&mut self) -> impl FnMut(Object) -> Result<(), Infallible> + '_ {
            |o| {
                self.0.insert(o.key, o.bytes);
                Ok(())
            }
        }
    }

    fn leaf(entries: &[Entry]) -> Object {
        encode_dir_leaf(entries).expect("encode leaf")
    }

    /// Builds a directory of `n` regular-file entries named e00000..e<n-1>
    /// into `store`, chunked into a multi-level prolly tree, and returns its
    /// root (Go lookup_test `bigDir`).
    fn big_dir(store: &mut MemStore, n: usize) -> Key {
        let blob = encode_blob(b"x");
        store.insert(&blob);
        let mut db = DirBuilder::new(ItemChunker::new(3));
        for i in 0..n {
            let e = Entry {
                name: format!("e{i:05}").into_bytes(),
                mode: 0o100644,
                content_key: blob.key.as_bytes().to_vec(),
                ..Default::default()
            };
            db.add_entry(&mut store.emit(), e).unwrap();
        }
        let root = db.finish(&mut store.emit()).unwrap();
        assert_eq!(root.type_(), Type::DirNode, "fixture root should promote");
        root
    }

    // --- ports of Go children_test.go ---

    #[test]
    fn child_keys_blob_has_none() {
        let o = encode_blob(b"data");
        let kids = child_keys(o.key, &o.bytes).unwrap();
        assert!(kids.is_empty());
    }

    #[test]
    fn child_keys_file_node() {
        let b1 = encode_blob(b"chunk one");
        let b2 = encode_blob(b"chunk two");
        let fnode = encode_file_node(&[b1.key, b2.key]);
        let kids = child_keys(fnode.key, &fnode.bytes).unwrap();
        assert_eq!(kids, vec![b1.key, b2.key]);
    }

    #[test]
    fn child_keys_dir_leaf() {
        let blob = encode_blob(b"file content");
        let l = leaf(&[Entry {
            name: b"f".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        let kids = child_keys(l.key, &l.bytes).unwrap();
        assert_eq!(kids, vec![blob.key]);
    }

    // --- ports of Go fstree/commit_test.go ---

    /// A commit of `tree` with the given parents (Go `commitObj`).
    fn commit_obj(msg: &str, tree: Key, parents: &[Key]) -> Object {
        let id = crate::commit::Identity {
            name: "Ann".into(),
            email: "ann@example.com".into(),
            when: 1,
            tz_offset: 60,
        };
        let c = crate::commit::Commit {
            tree,
            parents: parents.to_vec(),
            author: id.clone(),
            committer: id,
            message: msg.into(),
            signature: Vec::new(),
            public_key: Vec::new(),
        };
        let (key, bytes) = c.object().expect("commit encodes");
        Object { key, bytes }
    }

    /// Two trees and a diamond of commits over them (Go `history`):
    /// first(treeA) <- left(treeB), right(treeA) <- tip(treeB), a merge.
    /// Returns every object plus the keys of `first` and `tip`.
    fn history() -> (Vec<Object>, Key, Key) {
        let mut all = complete_tree();
        let root_a = all.last().unwrap().key;
        let blob_c = encode_blob(b"gamma");
        let root_b = leaf(&[Entry {
            name: b"c.txt".to_vec(),
            mode: 0o100644,
            content_key: blob_c.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        let first = commit_obj("first", root_a, &[]);
        let left = commit_obj("left", root_b.key, &[first.key]);
        let right = commit_obj("right", root_a, &[first.key]);
        let tip = commit_obj("merge", root_b.key, &[left.key, right.key]);
        let (first_key, tip_key) = (first.key, tip.key);
        all.extend([blob_c, root_b, first, left, right, tip]);
        (all, first_key, tip_key)
    }

    #[test]
    fn child_keys_commit() {
        let tree = leaf(&[]);
        let p1 = commit_obj("p1", tree.key, &[]);
        let p2 = commit_obj("p2", tree.key, &[]);
        let c = commit_obj("merge", tree.key, &[p2.key, p1.key]);
        // The tree, then the parents in recorded order.
        let kids = child_keys(c.key, &c.bytes).unwrap();
        assert_eq!(kids, vec![tree.key, p2.key, p1.key]);

        let err = child_keys(c.key, b"not a commit").unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "fstree: decoding Commit {}: decoding commit: unexpected EOF",
                c.key
            )
        );
    }

    #[test]
    fn reachable_keys_follows_history() {
        let (all, _, tip) = history();
        let store = MemStore::of(&all.iter().collect::<Vec<_>>());
        let keys = reachable_keys(tip, store.get()).unwrap();
        assert_eq!(keys[0], tip, "root first");
        let mut count: HashMap<Key, usize> = HashMap::new();
        for k in &keys {
            *count.entry(*k).or_default() += 1;
        }
        for o in &all {
            assert_eq!(
                count.get(&o.key).copied().unwrap_or(0),
                1,
                "{} ({}) must be listed exactly once",
                o.key,
                o.key.type_()
            );
        }
        assert_eq!(keys.len(), all.len());
    }

    #[test]
    fn check_complete_follows_history() {
        let (all, first, tip) = history();
        let store = MemStore::of(&all.iter().collect::<Vec<_>>());
        let visited = check_complete(tip, store.get(), store.has(), 4).unwrap();
        assert_eq!(visited.len(), all.len());

        // An ancestor commit is gone: a commit is an interior node, so this
        // is the getter's error, not a MissingObjectError.
        let rest: Vec<&Object> = all.iter().filter(|o| o.key != first).collect();
        let store = MemStore::of(&rest);
        match check_complete(tip, store.get(), store.has(), 4).unwrap_err() {
            WalkError::Read { key, .. } => assert_eq!(key, first),
            other => panic!("err = {other:?}, want Read of the missing ancestor"),
        }

        // A blob only the first commit's tree holds is gone: a missing leaf.
        let blob_b = all[1].key;
        let rest: Vec<&Object> = all.iter().filter(|o| o.key != blob_b).collect();
        let store = MemStore::of(&rest);
        let err = check_complete(tip, store.get(), store.has(), 4).unwrap_err();
        assert_eq!(
            err.missing_object().expect("MissingObjectError").key,
            blob_b
        );
    }

    // --- ports of Go collect_test.go ---

    #[test]
    fn collect_entries_dir_leaf() {
        let blob = encode_blob(b"hello");
        let want = [
            Entry {
                name: b"a".to_vec(),
                mode: 0o100644,
                mtime: 1,
                content_key: blob.key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"b".to_vec(),
                mode: 0o120777,
                mtime: 2,
                link_target: b"a".to_vec(),
                ..Default::default()
            },
        ];
        let l = leaf(&want);
        let store = MemStore::of(&[&l]);
        let got = collect_entries(l.key, store.get()).unwrap();
        assert_eq!(got, want.to_vec());
    }

    #[test]
    fn collect_entries_descends_dir_node() {
        let blob = encode_blob(b"x");
        let leaf1 = leaf(&[Entry {
            name: b"a".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        let leaf2 = leaf(&[Entry {
            name: b"z".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        let node = encode_dir_node(&[
            DirPair {
                sep_name: b"a".to_vec(),
                child_key: leaf1.key.as_bytes().to_vec(),
            },
            DirPair {
                sep_name: b"z".to_vec(),
                child_key: leaf2.key.as_bytes().to_vec(),
            },
        ])
        .unwrap();
        let store = MemStore::of(&[&leaf1, &leaf2, &node]);
        let got = collect_entries(node.key, store.get()).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, b"a");
        assert_eq!(got[1].name, b"z");
    }

    // A DAG whose DirNodes all point at one child would expand to fan²
    // entries. Collection must fail at the first repeated name instead.
    #[test]
    fn collect_entries_rejects_fan_in_dag() {
        let blob = encode_blob(b"x");
        let l = leaf(&[Entry {
            name: b"a".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        const FAN: usize = 4096;
        let mut pairs: Vec<DirPair> = (0..FAN)
            .map(|_| DirPair {
                sep_name: b"a".to_vec(),
                child_key: l.key.as_bytes().to_vec(),
            })
            .collect();
        let lower = encode_dir_node(&pairs).unwrap();
        for p in &mut pairs {
            p.child_key = lower.key.as_bytes().to_vec();
        }
        let upper = encode_dir_node(&pairs).unwrap();
        let store = MemStore::of(&[&l, &lower, &upper]);
        let gets = std::cell::Cell::new(0u32);
        let get = store.get();
        let counting = |k| {
            gets.set(gets.get() + 1);
            get(k)
        };
        collect_entries(upper.key, counting)
            .expect_err("expected an error for a DAG with repeated children");
        assert!(
            gets.get() <= 8,
            "{} object reads before rejecting; the DAG was being expanded",
            gets.get()
        );
    }

    #[test]
    fn collect_entries_rejects_out_of_order_leaves() {
        let blob = encode_blob(b"x");
        let leaf_a = leaf(&[Entry {
            name: b"a".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        let leaf_z = leaf(&[Entry {
            name: b"z".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        let node = encode_dir_node(&[
            DirPair {
                sep_name: b"z".to_vec(),
                child_key: leaf_z.key.as_bytes().to_vec(),
            },
            DirPair {
                sep_name: b"a".to_vec(),
                child_key: leaf_a.key.as_bytes().to_vec(),
            },
        ])
        .unwrap();
        let store = MemStore::of(&[&leaf_a, &leaf_z, &node]);
        let err = collect_entries(node.key, store.get())
            .expect_err("expected an error for leaves out of name order");
        assert!(
            matches!(err, WalkError::OutOfOrder { .. }),
            "got {err}, want OutOfOrder"
        );
    }

    #[test]
    fn collect_entries_rejects_non_dir_key() {
        let blob = encode_blob(b"data");
        let store = MemStore::of(&[&blob]);
        let err = collect_entries(blob.key, store.get()).unwrap_err();
        assert!(matches!(err, WalkError::NotDirObject { .. }));
        // Message pinned against the Go oracle.
        assert_eq!(
            err.to_string(),
            format!("fstree: {} is not a directory object (type Blob)", blob.key)
        );
    }

    /// The three-level fixture shared by the resolve tests.
    fn resolve_fixture() -> (Object, Object, Object, Object, MemStore) {
        let blob = encode_blob(b"x");
        let inner = leaf(&[Entry {
            name: b"file".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        let mid = leaf(&[
            Entry {
                name: b"inner".to_vec(),
                mode: 0o040755,
                content_key: inner.key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"link".to_vec(),
                mode: 0o120777,
                link_target: b"inner".to_vec(),
                ..Default::default()
            },
        ]);
        let root = leaf(&[
            Entry {
                name: b"file".to_vec(),
                mode: 0o100644,
                content_key: blob.key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"mid".to_vec(),
                mode: 0o040755,
                content_key: mid.key.as_bytes().to_vec(),
                ..Default::default()
            },
        ]);
        let store = MemStore::of(&[&blob, &inner, &mid, &root]);
        (blob, inner, mid, root, store)
    }

    #[test]
    fn resolve_path_cases() {
        let (_, inner, _, root, store) = resolve_fixture();

        // Empty path and "." resolve to the root itself.
        for p in ["", ".", "./"] {
            assert_eq!(resolve_path(root.key, p, store.get()).unwrap(), root.key);
        }

        // A nested path descends to the inner directory key ("mid" is a
        // single-leaf dir here; "mid/inner" names the inner dir's leaf).
        assert_eq!(
            resolve_path(root.key, "mid/inner", store.get()).unwrap(),
            inner.key
        );

        // Leading and trailing slashes are tolerated.
        assert_eq!(
            resolve_path(root.key, "/mid/inner/", store.get()).unwrap(),
            inner.key
        );

        // A missing component is NotFound.
        assert!(
            resolve_path(root.key, "mid/nope", store.get())
                .unwrap_err()
                .is_not_found()
        );

        // A non-directory component is NotDir.
        let err = resolve_path(root.key, "file", store.get()).unwrap_err();
        assert!(err.is_not_dir());
        assert_eq!(err.to_string(), "fstree: \"file\": not a directory");

        // ".." has no meaning in a CAS tree and is rejected.
        let err = resolve_path(root.key, "mid/..", store.get()).unwrap_err();
        assert!(matches!(err, WalkError::DotDot { .. }));
        assert_eq!(
            err.to_string(),
            "fstree: \"mid/..\": \"..\" is not supported"
        );
    }

    #[test]
    fn resolve_entry_cases() {
        let (_, _, _, root, store) = resolve_fixture();

        // The empty path (and "." chains) is the root itself: no entry.
        for p in ["", ".", "./"] {
            assert_eq!(resolve_entry(root.key, p, store.get()).unwrap(), None);
        }

        // A file at the end is returned with its metadata.
        let ent = resolve_entry(root.key, "mid/inner/file", store.get())
            .unwrap()
            .unwrap();
        assert_eq!(ent.name, b"file");
        assert_eq!(ent.mode, 0o100644);

        // A directory at the end is returned as its entry.
        let ent = resolve_entry(root.key, "mid/inner", store.get())
            .unwrap()
            .unwrap();
        assert_eq!(ent.name, b"inner");

        // A symlink at the end is returned, not followed.
        let ent = resolve_entry(root.key, "mid/link", store.get())
            .unwrap()
            .unwrap();
        assert_eq!(ent.link_target, b"inner");

        // Missing final component.
        assert!(
            resolve_entry(root.key, "mid/nope", store.get())
                .unwrap_err()
                .is_not_found()
        );

        // Descending through a non-directory.
        assert!(
            resolve_entry(root.key, "file/x", store.get())
                .unwrap_err()
                .is_not_dir()
        );

        // Descending through a symlink mid-path is NotDir too (symlinks are
        // never followed).
        assert!(
            resolve_entry(root.key, "mid/link/x", store.get())
                .unwrap_err()
                .is_not_dir()
        );

        // ".." is rejected.
        let err = resolve_entry(root.key, "mid/..", store.get()).unwrap_err();
        assert!(matches!(err, WalkError::DotDot { .. }));
    }

    // --- ports of Go lookup_test.go ---

    #[test]
    fn lookup_entry_big_dir() {
        let mut store = MemStore::default();
        let root = big_dir(&mut store, 1000);

        for name in ["e00000", "e00001", "e00499", "e00998", "e00999"] {
            let ent = lookup_entry(root, name.as_bytes(), store.get()).unwrap();
            assert_eq!(ent.name, name.as_bytes(), "LookupEntry({name})");
        }
        for name in ["", "a", "e004995", "e00999x", "zzz"] {
            let err = lookup_entry(root, name.as_bytes(), store.get()).unwrap_err();
            assert!(err.is_not_found(), "LookupEntry({name:?}) = {err}");
        }
        // Message pinned against the Go oracle.
        assert_eq!(
            lookup_entry(root, b"nope", store.get())
                .unwrap_err()
                .to_string(),
            "fstree: \"nope\": entry not found"
        );
    }

    #[test]
    fn lookup_entry_touches_few_objects() {
        let mut store = MemStore::default();
        let root = big_dir(&mut store, 1000);
        let mut reads = 0usize;
        let counting = |k: Key| {
            reads += 1;
            store.0.get(&k).cloned().ok_or("missing")
        };
        lookup_entry(root, b"e00500", counting).unwrap();
        // Tree depth for 1000 entries at average run 8 is ~4; anything near
        // O(n) means the descent is broken.
        assert!(reads <= 8, "lookup read {reads} objects");
    }

    #[test]
    fn lookup_entry_single_leaf() {
        let dir_key = leaf(&[]).key;
        let l = leaf(&[
            Entry {
                name: b"a".to_vec(),
                mode: 0o100644,
                content_key: encode_blob(b"x").key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"m".to_vec(),
                mode: 0o040755,
                content_key: dir_key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"z".to_vec(),
                mode: 0o120777,
                link_target: b"a".to_vec(),
                ..Default::default()
            },
        ]);
        let store = MemStore::of(&[&l]);
        let ent = lookup_entry(l.key, b"m", store.get()).unwrap();
        assert_eq!(ent.name, b"m");
        assert!(
            lookup_entry(l.key, b"b", store.get())
                .unwrap_err()
                .is_not_found()
        );
    }

    #[test]
    fn lookup_entry_rejects_non_dir() {
        let blob = encode_blob(b"data");
        let store = MemStore::of(&[&blob]);
        let err = lookup_entry(blob.key, b"a", store.get()).unwrap_err();
        assert!(matches!(err, WalkError::NotDirObject { .. }));
    }

    // --- ports of Go list_test.go ---

    #[test]
    fn list_entries_pages_match_collect() {
        let mut store = MemStore::default();
        let root = big_dir(&mut store, 1000);
        let want = collect_entries(root, store.get()).unwrap();

        for limit in [1usize, 7, 100, 999, 1000, 5000] {
            let mut got: Vec<Entry> = Vec::new();
            let mut after: Vec<u8> = Vec::new();
            loop {
                let (page, more) = list_entries(root, &after, limit, store.get()).unwrap();
                assert!(page.len() <= limit, "limit {limit}: page overflow");
                if !more {
                    got.extend(page);
                    break;
                }
                assert!(!page.is_empty(), "limit {limit}: more=true with empty page");
                after = page.last().unwrap().name.clone();
                got.extend(page);
            }
            assert_eq!(got, want, "limit {limit}");
        }
    }

    #[test]
    fn list_entries_more_flag() {
        let mut store = MemStore::default();
        let root = big_dir(&mut store, 1000);

        // limit == remaining entries: full page, nothing more.
        let (page, more) = list_entries(root, &[], 1000, store.get()).unwrap();
        assert_eq!((page.len(), more), (1000, false));

        // limit one short: more must be true.
        let (page, more) = list_entries(root, &[], 999, store.get()).unwrap();
        assert_eq!((page.len(), more), (999, true));

        // Cursor in the middle of a leaf run starts strictly after it.
        let (page, _) = list_entries(root, b"e00007", 3, store.get()).unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].name, b"e00008");

        // Cursor past the last entry: empty page, no more.
        let (page, more) = list_entries(root, b"e00999", 10, store.get()).unwrap();
        assert_eq!((page.len(), more), (0, false));
    }

    #[test]
    fn list_entries_empty_dir() {
        let l = leaf(&[]);
        let store = MemStore::of(&[&l]);
        let (page, more) = list_entries(l.key, &[], 10, store.get()).unwrap();
        assert_eq!((page.len(), more), (0, false));
    }

    #[test]
    fn list_entries_touches_few_objects() {
        let mut store = MemStore::default();
        let root = big_dir(&mut store, 1000);
        let mut reads = 0usize;
        let counting = |k: Key| {
            reads += 1;
            store.0.get(&k).cloned().ok_or("missing")
        };
        list_entries(root, b"e00500", 10, counting).unwrap();
        // A 10-entry page from the middle needs the root path plus a few
        // leaves; reading dozens of objects means subtree skipping is broken.
        assert!(reads <= 12, "page read {reads} objects");
    }

    #[test]
    fn list_entries_bad_limit() {
        let mut store = MemStore::default();
        let root = big_dir(&mut store, 100);
        let err = list_entries(root, &[], 0, store.get()).unwrap_err();
        assert!(matches!(err, WalkError::BadLimit { limit: 0 }));
        // Message pinned against the Go oracle. (Go also rejects negative
        // limits with the same message; usize makes those unrepresentable.)
        assert_eq!(
            err.to_string(),
            "fstree: ListEntries limit must be positive, got 0"
        );
    }

    #[test]
    fn list_entries_rejects_non_dir() {
        let blob = encode_blob(b"data");
        let store = MemStore::of(&[&blob]);
        assert!(matches!(
            list_entries(blob.key, &[], 10, store.get()).unwrap_err(),
            WalkError::NotDirObject { .. }
        ));
    }

    // --- write_content ---

    #[test]
    fn write_content_concatenates_blobs() {
        let b1 = encode_blob(b"hello ");
        let b2 = encode_blob(b"world");
        let fnode = encode_file_node(&[b1.key, b2.key]);
        let store = MemStore::of(&[&b1, &b2, &fnode]);
        let mut out = Vec::new();
        write_content(&mut out, fnode.key, store.get()).unwrap();
        assert_eq!(out, b"hello world");

        let mut out = Vec::new();
        write_content(&mut out, b1.key, store.get()).unwrap();
        assert_eq!(out, b"hello ");
    }

    #[test]
    fn write_content_rejects_non_content_object() {
        let l = leaf(&[]);
        let store = MemStore::of(&[&l]);
        let mut out = Vec::new();
        let err = write_content(&mut out, l.key, store.get()).unwrap_err();
        // Message pinned against the Go oracle: no "fstree:" prefix here.
        assert_eq!(
            err.to_string(),
            format!("{} is not a file-content object (type DirLeaf)", l.key)
        );
    }

    #[test]
    fn write_content_read_error_unprefixed() {
        let blob = encode_blob(b"x");
        let mut out = Vec::new();
        let err =
            write_content(&mut out, blob.key, |_k: Key| Err::<Vec<u8>, &str>("boom")).unwrap_err();
        assert_eq!(err.to_string(), format!("reading {}: boom", blob.key));
    }

    // --- ports of Go reachable_test.go ---

    /// Builds the walk fixture exercising every object type (Go
    /// `TestReachableKeys`).
    #[test]
    fn reachable_keys_all_types_dedup() {
        let blob_a = encode_blob(b"alpha");
        let blob_b = encode_blob(b"beta");
        let file_node = encode_file_node(&[blob_a.key, blob_b.key]);
        let mut xm = std::collections::BTreeMap::new();
        xm.insert(b"user.comment".to_vec(), b"hello".to_vec());
        let xattrs = encode_xattr_set(&xm);
        let sub_leaf = leaf(&[Entry {
            name: b"big".to_vec(),
            mode: 0o100644,
            content_key: file_node.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        // blob_a is referenced both here and as a FileNode child; it must be
        // listed once.
        let l = leaf(&[
            Entry {
                name: b"a.txt".to_vec(),
                mode: 0o100644,
                content_key: blob_a.key.as_bytes().to_vec(),
                xattrs_key: xattrs.key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"link".to_vec(),
                mode: 0o120777,
                link_target: b"a.txt".to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"sub".to_vec(),
                mode: 0o040755,
                content_key: sub_leaf.key.as_bytes().to_vec(),
                ..Default::default()
            },
        ]);
        let root = encode_dir_node(&[DirPair {
            sep_name: b"a.txt".to_vec(),
            child_key: l.key.as_bytes().to_vec(),
        }])
        .unwrap();
        let store = MemStore::of(&[&blob_a, &blob_b, &file_node, &xattrs, &sub_leaf, &l, &root]);

        let got = reachable_keys(root.key, store.get()).unwrap();
        let want: HashSet<Key> = [
            root.key,
            l.key,
            blob_a.key,
            xattrs.key,
            sub_leaf.key,
            file_node.key,
            blob_b.key,
        ]
        .into_iter()
        .collect();
        assert_eq!(got.len(), want.len());
        assert_eq!(got[0], root.key, "first key must be the root");
        assert_eq!(got.iter().copied().collect::<HashSet<_>>(), want);
    }

    #[test]
    fn reachable_keys_file_root() {
        let blob = encode_blob(b"alpha");
        let fnode = encode_file_node(&[blob.key]);
        let store = MemStore::of(&[&blob, &fnode]);
        let got = reachable_keys(fnode.key, store.get()).unwrap();
        assert_eq!(got, vec![fnode.key, blob.key]);
    }

    #[test]
    fn reachable_keys_missing_object() {
        let blob = encode_blob(b"alpha");
        let missing = leaf(&[Entry {
            name: b"x".to_vec(),
            mode: 0o100644,
            content_key: blob.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        let parent = leaf(&[Entry {
            name: b"gone".to_vec(),
            mode: 0o040755,
            content_key: missing.key.as_bytes().to_vec(),
            ..Default::default()
        }]);
        // The store holds only the parent: walking into "gone" must fail.
        let store = MemStore::of(&[&parent]);
        let err = reachable_keys(parent.key, store.get()).unwrap_err();
        assert!(matches!(err, WalkError::Read { .. }));
        assert_eq!(
            err.to_string(),
            format!(
                "fstree: reading {}: object {} not in store",
                missing.key, missing.key
            )
        );
    }

    #[test]
    fn reachable_keys_wide_parallel() {
        const N: usize = 64;
        let mut store = MemStore::default();

        // A blob shared by every subdirectory: discovered concurrently from
        // many parallel fetches in the same round, it must still be listed
        // once.
        let shared = encode_blob(b"shared");
        store.insert(&shared);

        let mut entries = Vec::new();
        for i in 0..N {
            let blob = encode_blob(format!("content-{i}").as_bytes());
            store.insert(&blob);
            let sub = leaf(&[
                Entry {
                    name: b"shared".to_vec(),
                    mode: 0o100644,
                    content_key: shared.key.as_bytes().to_vec(),
                    ..Default::default()
                },
                Entry {
                    name: b"uniq".to_vec(),
                    mode: 0o100644,
                    content_key: blob.key.as_bytes().to_vec(),
                    ..Default::default()
                },
            ]);
            store.insert(&sub);
            entries.push(Entry {
                name: format!("d{i:02}").into_bytes(),
                mode: 0o040755,
                content_key: sub.key.as_bytes().to_vec(),
                ..Default::default()
            });
        }
        let root = leaf(&entries);
        store.insert(&root);

        let got = reachable_keys(root.key, store.get()).unwrap();
        assert_eq!(got[0], root.key, "first key must be the root");
        // root + n sub-leaves + n unique blobs + 1 shared blob (deduped).
        assert_eq!(got.len(), 1 + 2 * N + 1);
        let uniq: HashSet<Key> = got.iter().copied().collect();
        assert_eq!(uniq.len(), got.len(), "duplicate keys in result");
    }

    // --- ports of Go checkcomplete_test.go ---

    /// Builds a small tree exercising every object type; returns all of its
    /// objects, root last.
    fn complete_tree() -> Vec<Object> {
        let blob_a = encode_blob(b"alpha");
        let blob_b = encode_blob(b"beta");
        let file_node = encode_file_node(&[blob_a.key, blob_b.key]);
        let mut xm = std::collections::BTreeMap::new();
        xm.insert(b"user.comment".to_vec(), b"hello".to_vec());
        let xattrs = encode_xattr_set(&xm);
        let l = leaf(&[
            Entry {
                name: b"a.txt".to_vec(),
                mode: 0o100644,
                content_key: blob_a.key.as_bytes().to_vec(),
                xattrs_key: xattrs.key.as_bytes().to_vec(),
                ..Default::default()
            },
            Entry {
                name: b"big".to_vec(),
                mode: 0o100644,
                content_key: file_node.key.as_bytes().to_vec(),
                ..Default::default()
            },
        ]);
        let root = encode_dir_node(&[DirPair {
            sep_name: b"a.txt".to_vec(),
            child_key: l.key.as_bytes().to_vec(),
        }])
        .unwrap();
        vec![blob_a, blob_b, file_node, xattrs, l, root]
    }

    #[test]
    fn check_complete_complete_tree() {
        let objs = complete_tree();
        let root = objs.last().unwrap().key;
        let store = MemStore::of(&objs.iter().collect::<Vec<_>>());
        let visited = check_complete(root, store.get(), store.has(), 4).unwrap();
        assert_eq!(visited.len(), objs.len(), "visited count");
        assert_eq!(visited[0], root, "visited[0] must be the root");
        let want: HashSet<Key> = objs.iter().map(|o| o.key).collect();
        let mut seen: HashSet<Key> = HashSet::new();
        for k in &visited {
            assert!(seen.insert(*k), "key {k} visited twice");
            assert!(want.contains(k), "unexpected visited key {k}");
        }
        for k in &want {
            assert!(seen.contains(k), "key {k} not visited");
        }
        // The list is deterministic (results are indexed by frontier
        // position), so every jobs setting yields the same order.
        assert_eq!(
            check_complete(root, store.get(), store.has(), 0).unwrap(),
            visited
        );
        assert_eq!(
            check_complete(root, store.get(), store.has(), 1).unwrap(),
            visited
        );
    }

    #[test]
    fn check_complete_missing_leaf() {
        let objs = complete_tree();
        let root = objs.last().unwrap().key;
        let missing = objs[1].key; // blob_b
        let present: Vec<&Object> = objs.iter().filter(|o| o.key != missing).collect();
        let store = MemStore::of(&present);
        let err = check_complete(root, store.get(), store.has(), 4).unwrap_err();
        let miss = err.missing_object().expect("want a MissingObjectError");
        assert_eq!(miss.key, missing);
        assert_eq!(
            err.to_string(),
            format!("fstree: object {missing} is missing")
        );
    }

    #[test]
    fn check_complete_missing_interior_node() {
        let objs = complete_tree();
        let root = objs.last().unwrap().key;
        let missing = objs[2].key; // file_node
        let present: Vec<&Object> = objs.iter().filter(|o| o.key != missing).collect();
        let store = MemStore::of(&present);
        // The wrapped get error must surface so callers can map it to their
        // own not-found (Go asserts errors.Is on the getter's sentinel).
        let err = check_complete(root, store.get(), store.has(), 4).unwrap_err();
        match err {
            WalkError::Read { key, ref source } => {
                assert_eq!(key, missing);
                assert_eq!(source, &format!("object {missing} not in store"));
            }
            other => panic!("err = {other:?}, want Read"),
        }
    }

    #[test]
    fn check_complete_has_error_passthrough() {
        let objs = complete_tree();
        let root = objs.last().unwrap().key;
        let store = MemStore::of(&objs.iter().collect::<Vec<_>>());
        let err = check_complete(
            root,
            store.get(),
            |_k| Err::<bool, String>("has-broke".into()),
            2,
        )
        .unwrap_err();
        assert!(matches!(err, WalkError::Has(_)));
        // Go returns the has error unwrapped; Display is transparent.
        assert_eq!(err.to_string(), "has-broke");
    }

    #[test]
    fn reachable_keys_order_matches_go_oracle() {
        // 251 shared blobs referenced by 5000 entries chunked at bits 2:
        // a deep DirNode tree with wide frontiers and heavy dedup. The
        // BLAKE3 digest over the returned keys IN ORDER is pinned against a
        // Go oracle run at the pinned commit — the walk's deterministic
        // frontier-position order must match Go's exactly.
        let mut store = MemStore::default();
        let mut blobs = Vec::with_capacity(251);
        for i in 0..251usize {
            let o = encode_blob(&vec![0u8; i]);
            blobs.push(o.key);
            store.insert(&o);
        }
        let mut db = DirBuilder::new(ItemChunker::new(2));
        for i in 0..5000usize {
            let e = Entry {
                name: format!("{i:06}").into_bytes(),
                mode: 0o100644,
                content_key: blobs[i % 251].as_bytes().to_vec(),
                ..Default::default()
            };
            db.add_entry(&mut store.emit(), e).unwrap();
        }
        let root = db.finish(&mut store.emit()).unwrap();
        assert_eq!(
            root.to_string(),
            "320e6e59d5ee3dff1246dfea67c179b006d6d18db614d295941794991f47dc38"
        );

        let keys = reachable_keys(root, store.get()).unwrap();
        assert_eq!(keys.len(), 1486, "reachable key count");
        let mut h = blake3::Hasher::new();
        for k in &keys {
            h.update(k.as_bytes());
        }
        let digest: String = h
            .finalize()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            digest, "9fd76cbbf8643a98d57cba4fcdd2601eac1035e847a4170291c41c13e86a44ca",
            "reachable_keys order differs from the Go oracle"
        );
    }

    // --- corrupt stored data (every message pinned against a Go oracle run
    // --- at the pinned commit) ---

    #[test]
    fn child_keys_rejects_short_entry_keys() {
        // A 31-byte content/xattrs key encodes fine (encode_dir_leaf parses
        // only exactly-32-byte keys) but child_keys must reject it.
        let l = leaf(&[Entry {
            name: b"f".to_vec(),
            mode: 0o100644,
            content_key: vec![0u8; 31],
            ..Default::default()
        }]);
        let err = child_keys(l.key, &l.bytes).unwrap_err();
        assert!(matches!(err, ChildKeysError::EntryContentKey { .. }));
        assert_eq!(
            err.to_string(),
            "fstree: \"f\": content key: key: data is not 32 bytes: got 31"
        );

        let l = leaf(&[Entry {
            name: b"g".to_vec(),
            mode: 0o100644,
            xattrs_key: vec![0u8; 31],
            ..Default::default()
        }]);
        let err = child_keys(l.key, &l.bytes).unwrap_err();
        assert!(matches!(err, ChildKeysError::EntryXattrsKey { .. }));
        assert_eq!(
            err.to_string(),
            "fstree: \"g\": xattrs key: key: data is not 32 bytes: got 31"
        );

        // Names render as {:?} of from_utf8_lossy (the crate convention):
        // control characters print as \u{1} where Go's %q prints \x01.
        let l = leaf(&[Entry {
            name: b"a\x01b".to_vec(),
            mode: 0o100644,
            content_key: vec![0u8; 31],
            ..Default::default()
        }]);
        let err = child_keys(l.key, &l.bytes).unwrap_err();
        assert_eq!(
            err.to_string(),
            "fstree: \"a\\u{1}b\": content key: key: data is not 32 bytes: got 31"
        );
    }

    #[test]
    fn corrupt_dir_node_child_key_errors() {
        let good_leaf = leaf(&[]);
        let good_node = encode_dir_node(&[DirPair {
            sep_name: b"z".to_vec(),
            child_key: good_leaf.key.as_bytes().to_vec(),
        }])
        .unwrap();
        // A DirNode body whose only pair holds a 31-byte child key:
        // [["z", 31 zero bytes]].
        let mut body = vec![0x81, 0x82, 0x41, b'z', 0x58, 0x1f];
        body.extend(std::iter::repeat_n(0u8, 31));
        let bad = |_k: Key| Ok::<_, Infallible>(body.clone());
        let want = format!(
            "fstree: child key in DirNode {}: key: data is not 32 bytes: got 31",
            good_node.key
        );

        let err = lookup_entry(good_node.key, b"a", &bad).unwrap_err();
        assert!(matches!(err, WalkError::ChildKey { .. }));
        assert_eq!(err.to_string(), want);

        let err = list_entries(good_node.key, &[], 5, &bad).unwrap_err();
        assert_eq!(err.to_string(), want);

        let err = collect_entries(good_node.key, &bad).unwrap_err();
        assert_eq!(err.to_string(), want);
    }

    #[test]
    fn corrupt_object_decode_errors() {
        let good_leaf = leaf(&[]);
        let good_node = encode_dir_node(&[DirPair {
            sep_name: b"z".to_vec(),
            child_key: good_leaf.key.as_bytes().to_vec(),
        }])
        .unwrap();
        let blob_a = encode_blob(b"a");
        let blob_b = encode_blob(b"bb");
        let good_file = encode_file_node(&[blob_a.key, blob_b.key]);
        let garbage = |_k: Key| Ok::<_, Infallible>(vec![0xffu8]);

        let err = collect_entries(good_node.key, garbage).unwrap_err();
        assert!(matches!(err, WalkError::DecodeDirNode { .. }));
        assert_eq!(
            err.to_string(),
            format!(
                "fstree: decoding DirNode {}: fstree: decoding dir node: \
                 cbor: unexpected \"break\" code",
                good_node.key
            )
        );

        let want_leaf = format!(
            "fstree: decoding DirLeaf {}: fstree: decoding dir leaf: \
             cbor: unexpected \"break\" code",
            good_leaf.key
        );
        let err = collect_entries(good_leaf.key, garbage).unwrap_err();
        assert_eq!(err.to_string(), want_leaf);
        let err = lookup_entry(good_leaf.key, b"a", garbage).unwrap_err();
        assert!(matches!(err, WalkError::DecodeDirLeaf { .. }));
        assert_eq!(err.to_string(), want_leaf);

        // write_content returns the FileNode decode error unwrapped (no
        // outer key wrap), exactly as Go does.
        let mut out = Vec::new();
        let err = write_content(&mut out, good_file.key, garbage).unwrap_err();
        assert!(matches!(err, WalkError::Codec(_)));
        assert_eq!(
            err.to_string(),
            "fstree: decoding file node: cbor: unexpected \"break\" code"
        );

        // ...while the walks wrap it with the FileNode's key.
        let want_file = format!(
            "fstree: decoding FileNode {}: fstree: decoding file node: \
             cbor: unexpected \"break\" code",
            good_file.key
        );
        let err = reachable_keys(good_file.key, garbage).unwrap_err();
        assert!(matches!(err, WalkError::Children(_)));
        assert_eq!(err.to_string(), want_file);
        let err =
            check_complete(good_file.key, garbage, |_k| Ok::<_, Infallible>(true), 2).unwrap_err();
        assert_eq!(err.to_string(), want_file);

        // A FileNode body whose only child bstr is 31 bytes: the child
        // index is part of the diagnostic.
        let mut body = vec![0x81, 0x58, 0x1f];
        body.extend(std::iter::repeat_n(0u8, 31));
        let err = write_content(&mut out, good_file.key, |_k| {
            Ok::<_, Infallible>(body.clone())
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "fstree: file node child 0: key: data is not 32 bytes: got 31"
        );
    }
}
