# vectorgen notes

`tools/vectorgen` (Go) generates every vector file in `tests/golden/` per
VECTORS.md, driving the pinned Go library (`github.com/amber-store/core`
at the version in `tools/vectorgen/go.mod` — v0.0.10 since the Go PR #15
backport; originally `jobs-build/amber-store-core@e4fcb60`; no replace
directive). Regenerate with `cd tools/vectorgen && go run . ../../tests/golden`
— the generator first deletes exactly the files it owns, so a rerun is a clean
rebuild.

## VECTORS.md corrections made

1. **Golden-tree metadata defaults.** The tree table left the root entries'
   mtimes (and the `sub`/`bigdir` directory entries' mtimes) unstated. The
   generator uses zero for every unstated metadata field; VECTORS.md now says
   so explicitly ("Any metadata field not stated is zero"). Without this the
   Rust builder could not reproduce the root key.
2. **Xattr spill rule source.** VECTORS.md pointed at `ingest/meta.go`; the
   rule actually lives in `ingest/driver.go` (`buildEntry`):
   `len(cborx.EncodeXattrs(m)) <= xattrInlineMax` (default 256) keeps the map
   inline, else it spills to an `XattrSet`. The threshold and ≤-comparison in
   VECTORS.md were already correct; only the file reference was fixed.
3. **amberignore check semantics.** The vector needs a defined answer for
   paths *under* an ignored directory. VECTORS.md now states the evaluation
   walk the generator (and the Rust test) must use — the ingest walk: an
   ignored ancestor directory prunes the subtree (path reports ignored, no
   re-inclusion below it); otherwise the final component's
   `Ignored(name, is_dir)` result is recorded.

## segments_go method

No crash simulation was needed. In the Go packstore, sealing happens **only**
when an append pushes the active segment to/past the size threshold
(`Store.append` → `sealActiveLocked`); `Store.Close` fsyncs and closes the
active segment **without sealing it** (`packstore.go`). So the generator:

1. `packstore.Open(dir, WithSegmentSize(65536), WithSync(false))`.
2. Sequentially `Put`s deterministic blob objects (mix of incompressible
   splitmix and compressible const payloads) until the directory holds two
   sealed `*.seg` files (checked by listing after each Put).
3. `Put`s three small tail objects — these create the third segment and stay
   far below 65536 bytes.
4. `Close`s the store, leaving `0000000000000003.seg.active` with a valid
   header + records and **no footer** — byte-identical to the "killed before
   seal" state VECTORS.md describes. Post-conditions asserted: exactly 2
   sealed + 1 active, active larger than the 8-byte header, no trailer magic
   at its end.

Sequential single-threaded `Put`s make the segment bytes a pure function of
the object sequence, so the whole directory is deterministic (the footer's
index/filter sections are order-independent by construction; zstd frames are
deterministic for the pinned `klauspost/compress` version).

## Other notes

- `records_raw.json`: the generator asserts flag byte == 0 for every case
  (splitmix payloads never win against zstd, including the empty payload);
  `records_compressed.json` asserts flag == 1.
- `filters.json` re-implements packstore's unexported `buildFilterSection`
  layout verbatim (type byte 0x01, BE seed/geometry/count, BE u16
  fingerprints) over `u64s(seed, n)` deduplicated + sorted.
- The golden tree is built via `chunkers.SplitBytes` → `fstree.EncodeBlob` →
  `fstree.NewFileIndexBuilder` (which returns a single chunk's blob key
  unwrapped, exactly like ingest) and `fstree.NewDirBuilder` over
  bytewise-name-sorted entries; empty files emit the empty Blob, mirroring
  `ingest/driver.go buildFile`.
- Verification performed after generation (throwaway program, since deleted):
  `objects.bin` keys parse canonically and their hash tails match
  BLAKE3(bytes); `pack_go.bin` stream-decodes to exactly the manifest objects
  in manifest order with a clean end marker; `pack_empty.bin` decodes to zero
  objects; a copy of `segments_go/` opens, serves all 31 manifest objects
  byte-exactly, and reports the 4 absent keys missing.
- Determinism check: generated twice into two scratch dirs; `diff -r` showed
  both runs identical to each other **and** to the committed
  `tests/golden/` output. JSON is emitted from structs only (fixed field
  order), never maps.

## Go PR #12 backport (2026-09-23)

`commit.go` writes `commit.json` (7 cases, see VECTORS.md). The pin moved
v0.0.7 → v0.0.9; a full regeneration into a scratch directory at the new pin
reproduced every existing file byte for byte, so only `commit.json` was
added.

## Go PR #13 backport (2026-09-23)

No vector changed: the reference store's file is not byte-reproducible and
has no vectors. The pin moved v0.0.9 →
`v0.0.10-0.20260923125338-91da3cf24ab7` (Go `91da3cf`, the merge of PR #13;
a pseudo-version, because the commit lies between two tags). A full
regeneration into a scratch directory at the new pin reproduced all 18 files
byte for byte.

## Go PR #14 backport (2026-09-23)

The pin moved on to `v0.0.10-0.20260923125444-1fb6953558f0` (Go `1fb6953`,
the merge of PR #14). A full regeneration into a scratch directory reproduced
every existing file byte for byte and added one:
`segments_go/0000000000000003.seg.active.idx`, the active segment's sidecar
index, which `genSegments` now asserts (232 bytes: magic, three entries, the
`synced` record of `Close`) and keeps. It deletes `gc.lock`, which every open
creates and which holds nothing a reader needs. `golden_segments_go_sidecar_is_trusted`
proves through the public API that Rust accepts the Go-written sidecar rather
than falling back to a scan; it was seen failing with the sidecar removed from
the fixture.

## Go PR #15 backport (2026-09-23)

`commit.go` writes 13 cases now: the seven from before — whose keys all
changed, the length field being a footprint, and whose bytes changed wherever
a parent's key is recorded — and six new ones (VECTORS.md): the conflicted
commit annotated in `architecture/commits.md`, a one-byte change id, every
key 0–9 at once, a conflict without labels whose term repeats, 254 terms with
255 labels, and identities with neither name nor email. The case struct
gained `change_id_hex`, `conflict_terms` and `conflict_labels`, omitted when
absent.

The pin moved on to `v0.0.10`, the release that contains PR #15; `commit.go`
needs `commit.Commit`'s new fields and does not build against anything
older. `commit.json` was first generated before that release existed,
through a temporary copy of `go.mod`/`go.sum` carrying `replace
github.com/amber-store/core => <a worktree at the PR's head>` and passed
with `go run -modfile=…`. At the `v0.0.10` pin a plain `go run .` into a
scratch directory reproduced it, and every other vector file, byte for byte.
