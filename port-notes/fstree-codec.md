# Port notes: `fstree` codec (Go `fstree/{object,encode,decode}.go`)

Scope: object types + encode/decode only. The builders and read paths
(`dir_builder.go`, `index_builder.go`, `children.go`, `collect.go`, …) are a
separate unit; `src/fstree/mod.rs` has a marked spot where their submodules
slot in.

## Files

- `src/fstree/mod.rs` — `Object`, `Entry`, `DirPair`, `Error` (+ re-exports).
- `src/fstree/encode.rs` — `encode_blob` / `encode_file_node` /
  `encode_dir_leaf` / `encode_dir_node` / `encode_xattr_set`, plus
  `pub(super) marshal_entries`/`marshal_pairs` (raw `encMode.Marshal`
  equivalents shared with tests).
- `src/fstree/fx.rs` — a faithful port of the **subset of fxamacker/cbor
  v2.9.2 behavior the Go decoders depend on** (see below); exports
  `CborError`/`CborType` publicly via the fstree module.
- `src/fstree/decode.rs` — `decode_file_node` / `decode_dir_leaf` /
  `decode_dir_node` wrappers.
- `tests/golden_fstree_codec.rs` — golden vectors (all 3444 objects).

## API mapping

| Go | Rust |
|----|------|
| `Object{Key, Bytes}` | `Object { key, bytes }` |
| `Entry` (cbor keyasint 0–9) | `Entry` (plain struct, `Default`) |
| `DirPair` (toarray) | `DirPair` |
| `Emit func(Object) error` | **not defined here** — the builders unit owns the emit-callback shape (PORTING.md prescribes closures/`FnMut`); defining it without its consumers would be speculative |
| `EncodeBlob` | `encode_blob` (infallible → returns `Object`) |
| `EncodeFileNode` | `encode_file_node` (infallible) |
| `EncodeDirLeaf` | `encode_dir_leaf -> Result<Object, Error>` |
| `EncodeDirNode` | `encode_dir_node -> Result<Object, Error>` |
| `EncodeXattrSet` | `encode_xattr_set` (infallible; takes `&BTreeMap<Vec<u8>, Vec<u8>>` to match `cbor::encode_xattrs`) |
| `DecodeFileNode/DirLeaf/DirNode` | `decode_file_node/dir_leaf/dir_node` |

Infallible encoders: the corresponding Go errors can only come from
`key.New` rejecting a reserved type, which the Rust `key::Type` enum makes
unrepresentable (same reasoning as `key::Key::new_from_hash`).

`Entry` optional fields are plain `Vec<u8>`/`Vec<u64>` with **empty ==
absent**, not `Option`: Go's `omitempty` treats nil and empty identically
(oracle-verified), so an `Option` would create a distinction the wire format
does not have.

## How exactness was established

Go's `decode.go` is three `cbor.Unmarshal` calls — its entire
strictness/laxness contract *is* fxamacker v2.9.2's default decode mode, and
`encode.go`'s `RawMessage` splice validation is fxamacker's Marshaler
re-check. Ported from the library source (`valid.go`, `decode.go`,
`stream.go`, `common.go`, `decode_map_utils.go` in the module cache) rather
than guessed, then verified two ways:

1. **Oracle probes**: ~150 handcrafted edge cases run through the real Go
   `fstree` at the pinned commit; every decoded value, re-encoded byte
   string, and error string is pinned in the unit tests (tables in
   `decode.rs`/`encode.rs`).
2. **Differential fuzzing**: 600 000 deterministically generated cases
   (300 k byte-level mutations of valid bodies + 300 k structure-aware
   generations hitting the struct decoder) through Go
   `Decode* → Encode*` and the Rust port side by side: **zero
   differences** in accept/reject decisions, decoded values, re-encoded
   bytes, keys, and full error strings. Coverage counters for round 2:
   21 599 struct-field errors, 27 696 builtin-tag errors, 19 733
   nested-level errors, 9 327 toarray count errors, 6 926 integer
   overflows, 2 773 UTF-8 rejections, 23 772 clean round-trips.
   (Throwaway harness deleted per the rules.)

## fxamacker behaviors deliberately reproduced (reviewer checklist)

- **Wellformed-first**: `Unmarshal` syntax-checks the whole input (incl.
  extraneous-data check) before any typed decode, so syntax errors beat type
  errors. Limits: nesting 32, array elements 131072, map pairs 131072;
  chained tags count toward nesting *after the first*.
- **Laxness**: indefinite lengths, non-shortest heads, tags (unwrapped),
  null/undefined → zero values, unknown map keys skipped, duplicate map
  keys keep the **first** value and skip later ones *without type checking*,
  text keys never match `keyasint` fields, unassigned simple values decode
  as their number, CBOR arrays decode element-wise into `[]byte`, bignum
  tags 2/3 fill byte-slices with their content bytes.
- **Strictness**: invalid UTF-8 in decoded text (even into `[]byte`, even as
  a value for a *matched* field) rejects; non-int/non-text map keys produce
  "cannot be used to match struct field name"; builtin tags 0–3 have their
  content types validated in the decode prologue; toarray pair count must be
  exactly 2.
- **Diagnostics byte-for-byte**, including Go type names (`[]uint8`,
  `fstree.Entry.4`, `fstree.DirPair.SepName`), the `Go's int64` vs `int64`
  overflow phrasings, the `-1-N` map-key rendering, and bignum decimal
  rendering (implemented with a small big-endian ÷10 loop).
- **Encode-side `RawMessage` check** (`raw_message_wellformed`): one
  well-formed item, indefinite lengths forbidden, builtin-tag content
  checked, limits maxed (nesting 65535 — hence the *iterative* wellformed
  implementation: fxamacker recurses, which Rust threads cannot afford at
  65535 frames). Errors render as
  `cbor: error calling MarshalCBOR for type cbor.RawMessage: …`.
- First error wins; decoding continues consuming items after an error
  exactly like Go's `parseArrayToSlice`/`parseMapToStruct` (observable only
  through which error is reported).

## Other decisions

- **Error type**: `fstree::Error` enum with one variant per Go wrap site
  (`PartialEq`, `std::error::Error::source`). `CborError` reproduces the
  fxamacker diagnostics; `Eof`/`UnexpectedEof` display bare (`EOF`,
  `unexpected EOF`) like Go's io errors.
- Go's `%q` on entry names is approximated with `{:?}` of
  `from_utf8_lossy` (same convention as `cbor.rs`). The rendered text differs
  from Go for non-UTF-8 names (`\u{fffd}` instead of `\xNN`) and for names
  containing non-printable characters (Rust `\u{1}` vs Go `\x01`); printable
  ASCII/Unicode names render identically. Diagnostic-only.
- Length-field arithmetic uses `wrapping_add` to match Go's silent `uint64`
  wraparound (Rust debug builds would otherwise panic).
- The typed-decode phase runs only after wellformed passes, so its low-level
  readers clamp instead of panicking (`debug_assert` + benign fallback) —
  unreachable by construction, but keeps the crate's no-panic rule against
  latent bugs.
- `EncodeDirLeaf` semantics preserved exactly: content/xattr keys are parsed
  (and counted) **only when exactly 32 bytes** — a 31-byte key is silently
  ignored; `EncodeDirNode` parses every child key unconditionally. Error
  ordering also preserved: the `Marshal` (RawMessage) error of a later
  entry beats the content-key error of an earlier one, because Go marshals
  first and sums after.

## Tests

- `cargo test` whole crate: green (182 lib + all integration tests).
- Unit tables: 7 file-node OK cases + 45 error cases; 88 dir-leaf cases;
  23 dir-node cases; 30+ encode vectors; RawMessage error strings; ports of
  all 8 Go `encode_test.go` tests. (Go has no decode tests to port; the
  oracle tables stand in.)
- Golden (`tests/golden_fstree_codec.rs`): parses `objects.bin` (3444
  objects), verifies manifest order/size/dedup/sorting, recomputes BLAKE3
  and checks the truncated hash + canonical key form for every object,
  checks the per-type length-field arithmetic, byte-compares decode →
  re-encode (and the recomputed key) for every FileNode/DirLeaf/DirNode,
  XattrSet via `cbor` xattrs, Blob via `encode_blob`; plus a second test
  asserting every referenced child/content/xattr key resolves within the
  set with the right type class.

## Adversarial review (independent pass)

- Re-read `fstree/{object,encode,decode}.go` and the fxamacker v2.9.2 sources
  (`valid.go`, `decode.go`, `encode.go`, `decode_map_utils.go`, `cache.go` in
  the module cache) side by side with `fx.rs`: wellformed limits/order, tag
  chain depth accounting, indefinite map odd-break, marshaler dec-mode
  (limits maxed, indef forbidden, tags allowed, builtin tags checked),
  keyasint text-key non-matching (`fieldIndicesByName` excludes keyasint
  fields), dup-field first-wins, and every fill-function error string were
  confirmed against the real library code. No drift found.
- Independent differential fuzz (own throwaway Go harness, deleted): 2 ×
  250 000 structure-aware/mutated inputs × all three decoders (750 000
  evaluations per round) through `Decode* → Encode*` on both sides, run in
  both release and debug (debug asserts + overflow checks): **zero
  mismatches**. The only diffs were the documented `%q` name-rendering
  deviation on garbage names (101/112 cases per round, suffixes identical).
- Added unit tests pinning marshaler-mode extremes unreachable via decode:
  `xattrs_in` nested 65535 deep is accepted (and cannot blow the stack —
  iterative wellformed), 65536 yields Go's exact `MarshalerError` string, and
  a 131073-element definite-array splice (over the decode cap, under the
  marshaler cap) encodes to the Go-oracle key.

## Open concerns

- `CborError` is public API surface (callers can match it); if later
  modules (`amberpack`/`reference`) need a shared "corrupt" classification,
  they wrap it at their layer — nothing here presumes one.
- The `fx` decoder implements exactly the shapes fstree needs. If the
  builders unit ever decodes other shapes it must extend `fx`, not
  hand-roll a second reader.
- fxamacker's `MaxArrayElements`/`MaxMapPairs` (131072) mean a *single*
  DirLeaf with >131072 entries would fail to decode in both Go and Rust —
  unreachable with the item chunker's run bounds, but worth remembering if
  builder parameters ever allow gigantic runs.

## Go PR #12 backport (2026-09-23)

`fx.rs` gained a Go `string` target (`parse_to_string`), generic
`keyasint`-struct helpers (`parse_to_struct`, `parse_map_to_struct`) and
`unmarshal_commit`, re-exported crate-internally for `src/commit.rs`. The
`Entry` decoder was left exactly as verified. `child_keys` gained the
`Commit` arm (tree, then parents in order) and
`ChildKeysError::DecodeCommit`. Rationale and the differential results are
in `port-notes/commit.md`.

## Go PR #15 backport (2026-09-23)

`fx.rs`: `commit.wireCommit` has ten fields now. Key 7 decodes through the
existing `[]uint8` target, key 8 through the existing `[][]uint8` target
(`parse_to_byte_slices`, which parents already used), key 9 through a new
`parse_to_strings`, which is `parse_to_slice_of("[]string",
parse_to_string)`: both halves existed and were verified, so no decode logic
is new. The rule worth knowing: a type error inside an array leaves with the
**element's** Go type (`string`, `[]uint8`) under the array's struct field
name, one against the array itself with the slice type (`[]string`,
`[][]uint8`); `parseArrayToSlice` keeps the first element error and decodes
on. The `Entry` decoder is untouched.

Read paths (`read.rs`): `dir_of`, the private `decode_commit` behind it and
`child_keys`, the readers' single call to `dir_of` on entry, and
`ChildKeysError::{CommitFootprint, CommitLength}`. `child_keys` of a commit
returns the tree, the conflict terms, then the parents. Rationale, the ported
Go quirks and the differential results are in `port-notes/commit.md`.
