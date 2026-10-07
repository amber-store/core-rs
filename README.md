# Amber-Store Core (Rust)

A content-addressable store for **filesystem trees** — arbitrarily deep
directories and files, where file content is split by content-defined chunking
and every object is identified by a fixed 32-byte key derived from a hash of
its content.

This crate is the Rust port of
[`github.com/amber-store/core`](https://github.com/amber-store/core)
(pinned at v0.9.0, see [`PORTING.md`](PORTING.md)), intended to be embedded as a **library** in other Rust
projects. The format is specified in [`architecture/`](architecture/).

From 0.9.0 on the crate carries the version of the Go release it is at parity
with: core-rs `X.Y.Z` and Go core `vX.Y.Z` read and write the same formats.

## Compatibility with the Go implementation

The port is **byte-compatible** at the content-addressing layer and
**interoperable** at the storage layer:

- **Byte-identical** (same input ⇒ same bytes): 32-byte keys, every serialized
  object (`Blob`/`FileNode`/`DirLeaf`/`DirNode`/`XattrSet`/`Commit`), UltraCDC
  and item-chunker cut points — hence identical root keys for identical trees
  and identical commit keys for identical commits — reference records,
  binary-fuse filter sections, segment footers, raw pack records, and PAX tar
  exports.
- **Interoperable, not byte-identical**: compressed record payloads. Go uses
  klauspost's zstd and pierrec's lz4, this crate libzstd and liblz4, and the
  two map compression levels differently; each decodes what the other wrote,
  but pack and segment files that contain compressed records differ byte-wise.
  Content addressing is unaffected, because keys hash the uncompressed bytes.
- **One shared file**: references live in `refs/refs.sqlite`, a SQLite
  database in WAL mode that both implementations open — at the same time, if
  need be. Its format and the rules for sharing it are specified in
  [`architecture/references.md`](architecture/references.md). A store
  written by an earlier release of this crate (a redb database) is imported
  on first open, so open it with this crate before the Go implementation
  touches it: Go knows nothing of `refs.redb` and would start an empty
  database next to it. One written by an earlier Go release (Pebble) has to
  be opened once by the Go implementation, which imports it.

See [`PORTING.md`](PORTING.md) for the full contract,
[`VECTORS.md`](VECTORS.md) for the Go-generated golden vectors that gate the
test suite, and [`interop/check.sh`](interop/check.sh) for a live
cross-implementation check (identical ingest roots and commit keys,
cross-reading each other's store directories, commits and references,
byte-identical exports).

## Library

The modules mirror the Go packages; see the crate docs (`cargo doc --open`).

| Module | Role |
|--------|------|
| `key` | The 32-byte content key: type, length, truncated BLAKE3 hash. |
| `fstree` | Tree objects (encode/decode), bottom-up builders, read paths. A commit key reads as its tree, as a root and as the content key of a directory entry. |
| `chunkers` | UltraCDC byte chunking and BLAKE3 item chunking. |
| `ingest` | Build a tree from a local directory; honors `.amberignore`; `Opts::exclude` skips names at the root (a working copy's metadata directory). |
| `amberignore` | `.gitignore`-semantics exclusion for ingestion. |
| `packstore` | Append-only pack segments with parallel, deduplicating, verifying writers. Any number of processes may read and write one store at once; see [`architecture/packstore.md`](architecture/packstore.md). |
| `refstore` | SQLite-backed (WAL, multi-process) name → record map for references, with optimistic updates; the file is shared with the Go implementation. |
| `reference` | The reference record: canonical CBOR encoding and validation. |
| `commit` | The commit record (object type 5): canonical CBOR encoding and validation; a change id and conflicted trees for jj; a key whose length field is the footprint of the snapshot (own bytes plus its trees); signature fields carried opaquely. See [`architecture/commits.md`](architecture/commits.md). |
| `amberpack` | The flat pack stream format for transfer and storage. |
| `inbox` | Durable pack receiving. |
| `tarexport` / `tarextract` | PAX tar streaming out of / into the store. |
| `binaryfuse` | Binary fuse filter, bit-compatible with FastFilter/xorfilter. |
| `cbor` | Shared deterministic-CBOR helpers (RFC 8949 §4.2). |

A minimal embedding:

```rust
use amber_store_core::{ingest, packstore, tarexport};

let store = packstore::Store::open(dir.join("packstore"), packstore::Options::new())?;
let (root, _stats) = ingest::dir(&store, "./some/dir", ingest::Opts::default())?;
tarexport::write(&mut out, root, |k| store.get(k))?;
```

Objects are stored uncompressed unless the store is opened with a compression
option:

```rust
use amber_store_core::amberpack::Compression;

let store = packstore::Store::open_with(
    dir.join("packstore"),
    packstore::Options::new().compression(Compression::Zstd { level: 0 }),
)?;
```

`Options::compression` takes none, zstd (levels 1–22) or lz4 (0 for the fast
compressor, 1–12 for high compression); level 0 is each algorithm's default.
`Options::compression_for` adds a function that chooses per object, from its
key and bytes. Every store reads records of every codec, whatever it was
opened with. Releases before this one read raw and zstd records only. A store
that has taken an lz4 record holds a segment at a format version they refuse,
so they cannot open that store at all; one that never has stays readable by
them. See [architecture/amberpack.md](architecture/amberpack.md). The example
CLI takes the setting as `--compression none|zstd[:LEVEL]|lz4[:LEVEL]`.

A dev CLI mirroring the Go `amber-store` commands
(ingest/ls/export/restore/ref/commit/gc) ships as an example: `cargo run --example amber-store -- --store ./store ingest DIR`.
A commit key, or a reference to one, works wherever a directory `KEY` does — it
stands for the commit's tree. So does a directory entry that holds a commit:
`ls`, `export` and `restore` skip the commit object and continue with its tree.
`commit create --change-id HEX` carries a change id; `commit show` prints it,
and the sides and labels of a conflicted tree.

## Development

```sh
nix develop          # rust toolchain + go (for regenerating golden vectors)
cargo test           # includes golden-vector suites under tests/
./interop/check.sh   # live Go↔Rust interop (needs the Go checkout; see the script)
```

Golden vectors are generated by the Go implementation
(`tools/vectorgen`, see `VECTORS.md`) and committed under `tests/golden/`.

## License

Licensed under the GNU Lesser General Public License, version 3 only
(`LGPL-3.0-only`). See [`LICENSE`](LICENSE) for the LGPL terms and
[`COPYING`](COPYING) for the GPL terms incorporated by the LGPL.

Third-party notices retain their stated licenses.
