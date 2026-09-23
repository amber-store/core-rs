//! Encodes the Amber-Store filesystem tree objects (`FileNode`, `DirLeaf`,
//! `DirNode`, `XattrSet`, `Blob`) as deterministic CBOR per
//! `architecture/fstree.md`, and builds files and directories bottom-up by
//! streaming. See `architecture/types.md` for the length-field semantics.
//!
//! The object-graph walks (`child_keys`, `reachable_keys`, `check_complete`)
//! also follow Commit objects (module [`crate::commit`]), whose children are
//! their trees — one, or every side of a conflict — and their parent commits.
//! The directory readers take a commit for the directory it records
//! ([`dir_of`]).
//!
//! Byte compatibility: every encoder in this module produces output identical
//! to the Go implementation (fxamacker/cbor v2.9.2 core-deterministic options
//! plus `NilContainerAsEmpty`), and every decoder accepts exactly what the Go
//! decoder accepts — including its laxness (indefinite lengths, non-shortest
//! heads, unknown or duplicate map keys, CBOR tags) and its exact diagnostic
//! messages. The compatible CBOR runtime lives in the private `fx` submodule.

mod builder;
mod decode;
mod encode;
mod fx;
mod read;

use std::fmt;

use crate::key;

pub use builder::{BuildError, DirBuilder, IndexBuilder};
pub use decode::{decode_dir_leaf, decode_dir_node, decode_file_node};
pub use encode::{
    encode_blob, encode_dir_leaf, encode_dir_node, encode_file_node, encode_xattr_set,
};
pub(crate) use fx::unmarshal_commit;
pub use fx::{CborError, CborType};
pub use read::{
    ChildKeysError, MissingObjectError, WalkError, check_complete, child_keys, collect_entries,
    dir_of, list_entries, lookup_entry, reachable_keys, resolve_entry, resolve_path, write_content,
};

/// A built CAS object: its key and its serialized bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// The 32-byte lookup key covering `bytes`.
    pub key: key::Key,
    /// The object's serialized bytes (raw content for a `Blob`).
    pub bytes: Vec<u8>,
}

/// A single directory entry, encoded as a canonical CBOR map with integer keys
/// 0–9 (`architecture/fstree.md`, DirLeaf).
///
/// Required keys 0–4 (`name`, `mode`, `uid`, `gid`, `mtime`) are always
/// encoded because uid/gid/mtime/mode are legitimately zero. The payload keys
/// 5–9 are omitted when empty and are mutually constrained by the caller per
/// the entry's type; an empty slice and Go's `nil` are the same "absent"
/// state, exactly as Go's `omitempty` treats them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// Key 0: single path component, raw bytes (no `/`).
    pub name: Vec<u8>,
    /// Key 1: raw POSIX `st_mode` (type + perms).
    pub mode: u64,
    /// Key 2: owner uid.
    pub uid: u64,
    /// Key 3: owner gid.
    pub gid: u64,
    /// Key 4: mtime in nanoseconds since the Unix epoch (may be negative).
    pub mtime: i64,
    /// Key 5: content key for `S_IFREG` / `S_IFDIR` entries; empty = absent.
    pub content_key: Vec<u8>,
    /// Key 6: inline symlink target for `S_IFLNK` entries; empty = absent.
    pub link_target: Vec<u8>,
    /// Key 7: `[major, minor]` for `S_IFCHR` / `S_IFBLK`; empty = absent.
    pub rdev: Vec<u64>,
    /// Key 8: inline xattrs as a pre-encoded canonical CBOR byte-string map
    /// (see [`crate::cbor::encode_xattrs`]), spliced into the entry verbatim
    /// (Go `cbor.RawMessage`); empty = absent.
    pub xattrs_in: Vec<u8>,
    /// Key 9: key of a spilled `XattrSet` object; empty = absent.
    pub xattrs_key: Vec<u8>,
}

/// One `[sepName, childKey]` element of a `DirNode`, encoded as a 2-element
/// CBOR array of byte strings (Go `toarray`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirPair {
    /// The highest entry name contained in the child's subtree.
    pub sep_name: Vec<u8>,
    /// The child object's 32-byte key (`DirLeaf` or `DirNode`).
    pub child_key: Vec<u8>,
}

/// Errors from the fstree codec, mirroring the Go package's error wrapping
/// (variant per wrap site, same diagnostic text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `fstree: decoding file node: <cbor error>`
    DecodeFileNode(CborError),
    /// `fstree: file node child <index>: <key error>`
    FileNodeChild {
        /// Zero-based child position within the node.
        index: usize,
        /// The key parse failure.
        source: key::Error,
    },
    /// `fstree: decoding dir leaf: <cbor error>`
    DecodeDirLeaf(CborError),
    /// `fstree: decoding dir node: <cbor error>`
    DecodeDirNode(CborError),
    /// `entry "<name>" content key: <key error>` (from [`encode_dir_leaf`]).
    EntryContentKey {
        /// The entry's name (Go renders it with `%q`).
        name: Vec<u8>,
        /// The key parse failure.
        source: key::Error,
    },
    /// `entry "<name>" xattrs key: <key error>` (from [`encode_dir_leaf`]).
    EntryXattrsKey {
        /// The entry's name (Go renders it with `%q`).
        name: Vec<u8>,
        /// The key parse failure.
        source: key::Error,
    },
    /// `dir node child key: <key error>` (from [`encode_dir_node`]).
    DirNodeChildKey(key::Error),
    /// `cbor: error calling MarshalCBOR for type cbor.RawMessage: <cbor
    /// error>` — an [`Entry::xattrs_in`] splice that is not one well-formed
    /// CBOR item (fxamacker validates `RawMessage` output on encode).
    MarshalRawMessage(CborError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::DecodeFileNode(e) => write!(f, "fstree: decoding file node: {e}"),
            Error::FileNodeChild { index, source } => {
                write!(f, "fstree: file node child {index}: {source}")
            }
            Error::DecodeDirLeaf(e) => write!(f, "fstree: decoding dir leaf: {e}"),
            Error::DecodeDirNode(e) => write!(f, "fstree: decoding dir node: {e}"),
            Error::EntryContentKey { name, source } => {
                write!(
                    f,
                    "entry {:?} content key: {source}",
                    String::from_utf8_lossy(name)
                )
            }
            Error::EntryXattrsKey { name, source } => {
                write!(
                    f,
                    "entry {:?} xattrs key: {source}",
                    String::from_utf8_lossy(name)
                )
            }
            Error::DirNodeChildKey(source) => write!(f, "dir node child key: {source}"),
            Error::MarshalRawMessage(e) => {
                write!(
                    f,
                    "cbor: error calling MarshalCBOR for type cbor.RawMessage: {e}"
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::DecodeFileNode(e)
            | Error::DecodeDirLeaf(e)
            | Error::DecodeDirNode(e)
            | Error::MarshalRawMessage(e) => Some(e),
            Error::FileNodeChild { source, .. }
            | Error::EntryContentKey { source, .. }
            | Error::EntryXattrsKey { source, .. }
            | Error::DirNodeChildKey(source) => Some(source),
        }
    }
}
