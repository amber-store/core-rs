//! Amber-Store core: a content-addressable store for filesystem trees.
//!
//! This is a byte-compatible Rust port of the Go implementation at
//! <https://github.com/jobs-build/amber-store-core>. Identical input trees
//! produce identical object bytes and identical 32-byte lookup keys; stores and
//! packs written by either implementation are readable by the other. The design
//! is specified in the `architecture/` documents in this repository.
//!
//! Module map (mirroring the Go packages):
//!
//! | Module | Role |
//! |--------|------|
//! | [`key`] | The 32-byte content key: type, length, truncated BLAKE3 hash. |
//! | [`cbor`] | Shared deterministic-CBOR helpers (RFC 8949 §4.2). |
//! | [`chunkers`] | Content-defined chunking: UltraCDC bytes + BLAKE3 item runs. |
//! | [`binaryfuse`] | Binary fuse filter (16-bit), bit-compatible with FastFilter/xorfilter. |
//! | [`fstree`] | Tree objects, bottom-up builders, and read paths. |
//! | [`amberignore`] | `.gitignore`-semantics exclusion for ingestion. |
//! | [`gc`] | Mark-and-sweep garbage collection: mark from the references, sweep by rewriting mostly-dead packs. |
//! | [`amberpack`] | The flat pack stream format: records + wire packs. |
//! | [`packstore`] | Append-only segment object store. |
//! | [`refstore`] | Name → reference-record map in SQLite (WAL, multi-process, optimistic updates); the file is shared with Go. |
//! | [`reference`] | The reference record: canonical CBOR encoding and validation. |
//! | [`commit`] | The commit record (object type 5): canonical CBOR encoding and validation. |
//! | [`inbox`] | Durable pack receiving. |
//! | [`ingest`] | Build a tree from a local directory. |
//! | [`tarexport`] | Stream a stored tree as a PAX tar. |
//! | [`tarextract`] | Materialize such a tar onto the filesystem. |

pub mod amberignore;
pub mod amberpack;
pub mod binaryfuse;
pub mod cbor;
pub mod chunkers;
pub mod commit;
pub mod fstree;
pub mod gc;
pub mod inbox;
pub mod ingest;
pub mod key;
pub mod packstore;
pub mod reference;
pub mod refstore;
pub mod tarexport;
pub mod tarextract;
