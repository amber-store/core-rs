# commit — port notes

Backport of Go PR #12 (amber-store/core, merge commit `e318780`, tag
`v0.0.9`, 2026-09-23): the `Commit` object, CAS type 5. Port of Go
`commit/commit.go` plus the touch points in `key`, `fstree`, `packstore`
and the dev CLI. The format is specified in `architecture/commits.md`
(copied verbatim from the Go repository, like the rest of `architecture/`).

Public API (`src/commit.rs`): `Commit` and `Identity` (public fields),
`Commit::encode` / `object` / `signature_payload` / `decode` / `trees` /
`conflicted`, the free function `footprint`, the error enums `Error`,
`ConvertError`, `IdentityError`, `TextError`, and the constants
`MAX_PARENTS`, `MAX_IDENTITY_LEN`, `MAX_MESSAGE_LEN`, `MAX_SIGNATURE_LEN`,
`MAX_PUBLIC_KEY_LEN`, `MAX_TZ_OFFSET`, `MAX_CHANGE_ID_LEN`,
`MAX_CONFLICT_TERMS`, `MAX_LABEL_LEN`. (The jj fields, the footprint and what
follows from them are the Go PR #15 backport: see the section at the end.)

| Go | Rust |
|----|------|
| `Commit{Tree, Parents, Author, Committer, Message, Signature, PublicKey, ChangeID, ConflictTerms, ConflictLabels}` | `Commit { tree, parents, author, committer, message, signature, public_key, change_id, conflict_terms, conflict_labels }` |
| `Identity{Name, Email, When, TZOffset int}` | `Identity { name, email, when, tz_offset: i32 }` |
| `(Commit).Encode` / `Object` / `SignaturePayload` | `encode` / `object -> (Key, Vec<u8>)` / `signature_payload` |
| `(Commit).Trees` / `Conflicted` / `Footprint(own, trees)` | `trees -> Vec<Key>` / `conflicted` / `commit::footprint(own, &trees) -> Result<u64, Error>` |
| `Decode` | `Commit::decode` |
| `wireCommit` / `wireIdentity` (unexported) | `WireCommit` / `WireIdentity` (`pub(crate)`) |

## Modeling decisions

- **`tree`/`parents` are `Key`s,** as in Go (unlike `Reference.key`, a byte
  vector on both sides). A Rust `Key` can hold non-canonical bytes — its inner
  array is public — so `validate` calls `Key::validate` before `Key::type_`,
  which panics on a reserved type nibble. The order (validate, then type
  check) is Go's.
- **`tz_offset` is `i32`.** Go's field is an `int`; the wire integer is an
  `int64`, range-checked to ±1439 during wire conversion before it is
  narrowed, exactly where Go narrows it. `IdentityError::TzOffset` carries an
  `i64` so the out-of-range wire value prints as Go prints it.
- **`message`, `name`, `email` are `String`s,** so Go's three "must be valid
  UTF-8" branches are unreachable on the encode side; the variants are kept
  with the verbatim Go messages (the `reference` precedent). On the decode
  path invalid UTF-8 is rejected during unmarshalling, where fxamacker rejects
  it too.
- **`signature`/`public_key`/`change_id`/`conflict_terms`/`conflict_labels`:
  empty = absent** (Go `omitempty`), as in `Reference`. Go tells a nil slice
  from an empty one in memory; nothing observable depends on it (see "Go
  quirks ported" in the PR #15 section).
- **`Error::Convert` vs `Error::Invalid`.** Go wraps two different failures in
  the same `"invalid commit: %w"`: `wireCommit.commit()`'s conversion errors
  (`tree: …`, `parent N: …`, `author: tz offset …`) and `validate()`'s
  (`commit tree …`). Both variants render that prefix; the split keeps each
  inner error typed.
- Go's defensive `"re-encoding commit: %w"` wrap is unreachable there and has
  no counterpart here (the Rust encoder is infallible).

## Decode: where the lax unmarshal lives

Go's `Decode` is `cbor.Unmarshal` (default mode) → `wireCommit.commit()` →
`validate()` → re-encode → byte-compare. The last step makes the accept set
"canonical encodings of valid commits" by construction; everything before it
only decides *how* a rejection is classified and worded.

The unmarshal is **not** hand-rolled in `commit.rs` the way `reference.rs`
does it. It is three additions to `fstree::fx`, the crate's faithful port of
fxamacker's decoder:

- `Dec::parse_to_string` — a Go `string` target under the default
  `ByteStringToStringForbidden`: only a text string fills it, null/undefined
  are a no-op, bignum tags take the integer paths and end as type errors,
  other tags unwrap.
- `Dec::parse_to_struct` / `Dec::parse_map_to_struct` — generic mirrors of
  `parseToValue`-into-struct and `parseMapToStruct` for `keyasint` structs.
  They are the same arms and the same loop as `parse_to_entry` /
  `parse_map_to_entry`, which predate them and were left untouched: that code
  was verified against 600 000 oracle cases and this backport had no reason
  to disturb it. Moving `Entry` onto the generic helpers is a possible later
  cleanup.
- `unmarshal_commit`, with `commit.wireCommit.N` / `commit.wireIdentity.N`
  field names.

Two consequences worth knowing:

- **No indefinite-length divergence.** `reference.rs` documents one (its head
  reader rejects indefinite heads early). `fx` decodes indefinite items as
  fxamacker does, so an indefinite-length parents array decodes and is then
  rejected as "not canonical", like Go.
- **The outer field name wins.** Both of fxamacker's struct wrap sites
  overwrite `UnmarshalTypeError.StructFieldName` unconditionally, so a type
  error inside a nested identity surfaces as
  `… into Go struct field commit.wireCommit.2 of type string` — the outer
  field, the inner Go type. `wrap_field` already overwrote; the nesting makes
  it observable for the first time. Pinned by a unit test.

`append_tstr` and `append_int` moved from `reference.rs` into `cbor.rs` so
both record codecs share them.

## Differential verification (harness deleted afterwards)

A throwaway Go oracle (a scratch module with a `replace` onto the Go checkout
at `e318780`) and a throwaway Rust integration test:

- **34 355 decode cases**: the three base encodings; a field × value matrix
  over every top-level key and every identity key with ~90 CBOR shapes
  (integers at every boundary, byte/text strings incl. invalid UTF-8, arrays
  that fill byte slices element-wise, maps, null/undefined/bool/floats,
  assigned and unassigned simple values, built-in tags with good and bad
  content, bignums in and out of range, self-described and unknown tags,
  indefinite-length strings/arrays/maps, reserved heads, nesting at 30/31/32);
  canonical values re-wrapped in tags and non-minimal heads; map-key shapes
  appended and prepended; duplicate keys before and after, missing keys;
  document-level shapes; parent counts around 23/24/255/256/257; truncation at
  every length; five substitutions, a deletion and six insertions at every
  offset; 18 000 multi-byte mutations; 3 000 random byte strings; 8 000 random
  structured documents. Verdict classes Go produced: 7 689 accept, 24 645
  decode-stage, 1 406 invalid, 615 not canonical. **Zero verdict mismatches
  and zero message mismatches**: every error string is byte-identical.
- **235 encode cases** (tree and parent keys of every type and header shape,
  parent counts, 19 identity strings in all four positions, timestamps and
  offsets at every integer-width boundary, message/signature/public-key sizes
  at and past every bound, several rules broken at once to pin the check
  order): 96 rejected by Go; bytes, keys, signature payloads and error strings
  all identical, and every accepted encoding decodes back to the same commit.

The gc and walk tests were additionally checked the other way round: with
the `Commit` arm of `child_keys` temporarily returning no children, four of
the five history tests fail; restored, all pass.

## Tests

| Go | Rust |
|----|------|
| `commit/commit_test.go` | `src/commit.rs` unit tests, same cases and the same pinned golden vector; the two invalid-UTF-8 encode cases are unrepresentable |
| `TestNewFromHash_Commit`, type tests | `src/key.rs` |
| `fstree/commit_test.go` | `src/fstree/read.rs` (`child_keys_commit`, `reachable_keys_follows_history`, `check_complete_follows_history`) |
| `TestVerifyObjectChecksCommitLength` | `src/packstore/verify.rs` |
| `gc/commit_test.go` | `src/gc/tests.rs` (`commit_history_stays_live`, `prepare_ref_missing_ancestor_fails`) |
| `TestE2E_Commit` | `tests/cli_e2e.rs::commit_end_to_end` |
| `TestParseIdentity`, `TestRenderCommit` | `tests/cli_e2e.rs::commit_identity_and_rendering`, through the CLI (the example has no unit tests) |
| — | `tests/golden_commit.rs` over `tests/golden/commit.json` (7 Go-generated cases) |

## Dev CLI

`commit create` / `commit show` and the commit peel in `descend` mirror Go's
`commit.go` and `spec.go`, including the error texts and the
"commit stored … but setting reference … failed" recovery hint. Deviations:

- **RFC 3339 is parsed by hand** (the example has no date dependency):
  `YYYY-MM-DDTHH:MM:SS[.frac](Z|±hh:mm)`, ranges checked, fraction truncated
  to nanoseconds. Go's `time.Parse` error wording is not reproduced, and a
  date outside the int64-nanosecond range is rejected where Go's `UnixNano`
  would return an undefined value.
- The default time is `now` with the local zone's offset from
  `localtime_r`'s `tm_gmtoff`, whole minutes, as Go's `when.Zone()`.
- Go's `%q` is approximated with `{:?}`, as elsewhere in the example.
- `renderCommit`'s `signature N bytes` line is ported but not reachable from a
  test: the CLI cannot create signed commits.

`interop/check.sh` gained a commit section: both CLIs create the same two
commits (fixed `--date`) and must print identical keys, and each shows and
lists the commits the other wrote.

## Release

Crate version 0.3.0 → 0.4.0: a new public module, a new `key::Type` variant
and a new `fstree::ChildKeysError` variant (exhaustive matches on either enum
need a new arm). The Go pin moved to `e318780` / v0.0.9 in `PORTING.md`, the
CI interop job and `tools/vectorgen/go.mod`.

## Go PR #15 backport (2026-09-23): jj fields, footprint length, commits inside directories

Port of `git diff e318780 cee003f` in the Go repository (PR #15, branch
`commit-jj`, its head before the merge). Three things: what a jj commit
carries besides git's fields, a **footprint** in the key's length field, and a
commit as the content key of a directory entry. The format is in
`architecture/commits.md`, `types.md`, `fstree.md` and `keys.md`, copied byte
for byte from the Go tree at `cee003f`. Go's design notes are
`docs/superpowers/specs/2026-09-23-commit-jj-design.md` and the "Review fixes"
at the end of the matching plan; they explain the old-rule refusal, the
DirNode rule and the 64 KiB label bound.

### What changed, by place

- **`src/commit.rs`**: wire keys `7 change_id` (bytes, 1..=64), `8
  conflict_terms` (array of 32-byte keys: even count, at most 254, each a
  canonical DirLeaf/DirNode key, repeats allowed), `9 conflict_labels` (array
  of text: only with key 8, exactly `1 + len(terms)`, each at most 64 KiB with
  no code point below U+0020 and no U+007F, at least one non-empty). An
  identity's name may be empty. `trees()`, `conflicted()`, `footprint()`.
  `object()` keys the commit with own bytes + the length field of every tree
  in `trees()`; parents are not counted. Validation appends to Go's order:
  … public key, change id, term count, each term (canonical, then type), then
  the labels (present without a conflict, count, each label's text, all
  empty).
- **API breaks** (the crate version is the coordinator's to bump):
  `TextError::TooLong` became `TooLong(usize)`, carrying the bound, because
  Go's `validateText` grew a `max` parameter and one message serves two
  bounds; `IdentityError::NameEmpty` is gone with the rule; `Commit` has three
  more public fields (there is no `Default`, `Key` has none, so every literal
  names them); new variants `ConvertError::ConflictTerm` and `Error::
  {ChangeIdTooLong, ConflictTermCount, ConflictTerm, ConflictTermNotDirectory,
  LabelsWithoutConflict, LabelCount, Label, LabelsAllEmpty,
  FootprintOverflow}`; `fstree::ChildKeysError::{CommitFootprint,
  CommitLength}`.
- **`src/fstree/fx.rs`**: `WIRE_COMMIT_FIELDS` grew to ten names and
  `parse_to_wire_commit` to ten arms. Key 7 is the existing `[]uint8` target,
  key 8 the existing `[][]uint8` target (parents use it), key 9 a new
  `parse_to_strings`, which is nothing but
  `parse_to_slice_of("[]string", parse_to_string)`: both halves existed, so
  no decode logic is new and the verified paths are untouched.
- **`src/fstree/read.rs`**: `child_keys` of a commit returns the tree, the
  conflict terms, then the parents. New private `decode_commit(k, data)`
  behind `child_keys` and `dir_of`: decode, then hold `k.length()` to the
  footprint. New public `dir_of(k, get)`. `lookup_entry`, `list_entries` and
  `collect_entries` call `dir_of` once on entry and nowhere further down, so
  a commit as the child of a DirNode is refused like any non-directory key
  (Go undid an earlier version that read through it: "Review fixes").
  `resolve_path`/`resolve_entry` needed no change: they re-enter
  `lookup_entry` per component, so a commit in an `S_IFDIR` entry is read
  through, and `resolve_path` returns the entry's key as stored, which may be
  a commit's.
- **`src/packstore/verify.rs`**: see `port-notes/packstore.md`.
- **`src/tarexport.rs`**: `write` accepts a Commit root; everything else comes
  from `collect_entries`.
- **gc**: no production change; one ported test (`port-notes/gc.md`).
- **`key`**: a doc comment (`port-notes/key.md`).
- **dev CLI**: `peel_commit` is gone. `descend` returns the final content key
  as stored and refuses a Commit under an entry that is not `S_IFDIR`;
  `commit create` resolves TREE with `fstree::dir_of`, takes `--change-id
  HEX` (refused when given but empty or not hex) and verifies every parent
  through `child_keys` before storing; `commit show` prints
  `conflict-remove`/`conflict-add` after `tree`, `conflict-label N TEXT` for
  each non-empty label, `change-id HEX` after the parents; an identity with
  neither name nor email prints the time alone.

### Go quirks ported (none of them "fixed")

- **Absence has one encoding, and it is the canonical check that says so.**
  `08 80`, `09 80` and `07 40` are neither decode errors nor validation
  errors: they unmarshal, convert, pass `validate` (every new rule is guarded
  by a length) and only then fail the re-encode comparison, so the verdict is
  `commit encoding is not canonical`. Go gets there through nil-versus-empty
  slices and `omitempty`; an empty `Vec` gives the same class with no such
  distinction to model. `08 f6` (null) under a commit that has labels is
  different: the terms are absent, so validation speaks first, with `commit
  has conflict labels but no conflict`.
- **Laxness inside the new arrays.** A `null` label is not a type error: it
  leaves Go's zero value, the empty string, and the record then fails the
  canonical check. A `null` term becomes an empty byte string and fails
  conversion (`conflict term N: key: data is not 32 bytes: got 0`). A CBOR
  array of small integers fills a byte string element-wise, and a bignum tag's
  content fills one directly, under key 7 and inside key 8 alike. A type
  error inside an array carries the **element's** Go type under the array's
  field name (`… into Go struct field commit.wireCommit.9 of type string`),
  one against the array itself the slice type (`… of type []string`,
  `… of type [][]uint8`). The first element error wins and decoding goes on.
- **Two stages, two prefixes for a term**, as for a parent: bytes that are
  not a canonical 32-byte key fail conversion (`invalid commit: conflict term
  0: key: …`), while a canonical key of the wrong type fails validation
  (`invalid commit: commit conflict term 0: … is not a directory key`). The
  encode side only has the second prefix.
- **Conversion order** is tree, parents, author, committer, *then* terms: a
  committer's tz offset out of range is reported before a malformed term.
- **The label messages.** `commit conflict label N <rule>` has no colon,
  unlike `commit conflict term N: …`. The count message names `1 +
  len(terms)` as "terms": `commit has 2 conflict labels for 3 terms`.
- **Control characters** are exactly the code points below U+0020 and U+007F,
  in labels as in identity strings. C1 controls, U+2028 and U+FEFF pass, so
  `char::is_control` would be the wrong test (Go's review notes say the same
  about a port). Pinned in `encode_accepts_jj_bounds`.
- **`decode` has no opinion on the footprint.** A record whose trees'
  lengths overflow 64 bits encodes and decodes; `object()` refuses to key it,
  and `child_keys`, `dir_of` and `verify_object` refuse whatever key it is
  stored under (`fstree: Commit K: commit footprint overflows …`).
- **`decode_commit` runs behind every walk**, so a commit keyed by v0.0.9's
  own-bytes rule is refused by `child_keys`, `dir_of`, the three readers,
  `reachable_keys`, `check_complete` (hence `prepare_ref` and `ref set`), the
  gc mark, `ls`/`export`/`restore` and `commit create --parent`, all with the
  one message that names the rule. `commit show` decodes without a key check
  and still prints such a commit, deliberately: that is how its tree is found
  again.
- **`verify_object` hashes first**, so a wrong hash is reported as such even
  when the bytes are no commit; then bytes under a Commit key that do not
  decode strictly fail verification whatever their length.
- **CLI**: `descend` no longer looks at the root at all, so a spec that names
  a file now fails in the reader with fstree's `… is not a directory object`,
  and `commit create FILE` fails in `dir_of` with that text instead of commit
  validation's. An entry that is not `S_IFDIR` but holds a Commit key is
  refused by `descend` by name; the same shape met by `export` further down
  fails with tarexport's `… is not a file-content object (type Commit)`,
  as in Go.

### Deviations from Go

- **`dir_of` reports a commit's failures as `WalkError::Children`**, the
  variant that carries `child_keys` errors, instead of growing `WalkError`
  by three variants that would duplicate `ChildKeysError`'s. Go has one
  untyped error there; the text is identical.
- **`ChildKeysError::CommitLength { key, want, own }`** stores what Go only
  formats; the found length is `key.length()`.
- **Invalid UTF-8 is unrepresentable on the encode side** (`String`), so Go's
  "invalid UTF-8 label" encode case has no counterpart, like its two
  predecessors. On the wire it is rejected at the decode stage with
  fxamacker's `cbor: invalid UTF-8 string`, as in Go, and pinned there.
- **`--change-id` is decoded by a small `go_hex_decode`** in the example that
  reproduces `encoding/hex`'s two errors (`invalid byte: U+0078 'x'`, a bad
  character reported before an odd length, `%#U`'s printable rule for bytes
  taken as code points). `parse_hex_key` keeps the `hex` crate's wording, as
  before; only the new flag was held to Go's text.
- **Tests.** Go's `TestRenderCommitConflicted` and
  `TestIdentityLineWithoutNameOrEmail` are unit tests of unexported
  functions; here `commit show` prints commits stored through the library
  (`commit_show_conflicted_and_nameless`), byte for byte Go's expectation.
  `commit_inside_a_directory` reads the exported archive back with
  `tarextract::extract`, there being no tar reader among the dev
  dependencies.

### Differential verification (harness deleted afterwards)

A throwaway Go module with `replace github.com/amber-store/core =>` a
worktree at `cee003f` wrote JSONL verdicts; a throwaway `tests/zz_*.rs`
compared verdict class, the full error string, bytes and keys, case by case.
Final run: **zero differences**. The harness was then shown to have teeth:
four seeded mutants (a Go type name in `fx`, one validation message, the
child order, one verification message) produced 16 484, 66 and 28 mismatches
in the three suites.

- **69 414 decode inputs**: five base encodings (resolved, the conflicted
  vector, everything, change id only, conflict without labels); a field ×
  value matrix of 137 CBOR shapes over keys 0–11 and 23, in canonical
  position and appended out of order, and under odd map keys (negative, text,
  bytes, null, overflowing); the same matrix inside both identities; integers
  at every boundary, byte and text strings including invalid UTF-8 (overlong,
  surrogates, truncated), indefinite-length strings, arrays and maps,
  non-minimal heads, nested type errors inside the arrays (integer, bytes,
  text, null, undefined, map, array, bignum, tagged, negative overflow),
  element-wise byte filling, bignums as byte strings, reserved heads, nesting
  at 30–33; tags and re-wrapped values; every entry removed, duplicated before
  and after with seven values, swapped with its neighbour; change ids of 0–1000
  bytes; 0–257 terms, with and without labels; a tree, a term and a parent of
  every key type and length-field shape, reserved type nibbles, the reserved
  bit, a leading-zero length, 31/33/0 bytes; every label count against 0, 2
  and 4 terms, all-empty and single-set labels; every control character,
  U+007F, C1, U+2028, U+FEFF, labels at 65 535/65 536/65 537 bytes (and
  multi-byte at the bound), in each label position and as identity strings and
  message; 36 records that break several rules at once, to pin the check
  order across stages; footprints that overflow; truncation at every length,
  five substitutions, a deletion and six insertions at every offset of every
  base; 18 000 multi-byte edits, 3 000 random byte strings, 12 000 random
  structured documents. Go's verdicts: 7 272 accept, 48 327 decode-stage,
  4 618 invalid, 9 197 not canonical. For **every** input, `child_keys` under
  the own-bytes key (the v0.0.9 rule) and, when it decodes, under the honest
  key; for every input up to 2 KiB, `put_verified` under both.
- **1 580 encode cases** over the conflicted vector and a labelled merge
  (1 036 rejected by Go): Go's unit-test cases, term counts 0–258, every odd
  key as tree, term and parent, label counts and texts, change-id lengths, 18
  order cases, empty-but-non-nil optionals, and the footprint's edge (a sum of
  exactly 2^64 − 1, one and two past it, in both term orders). Encode, object
  (key or error) and signature payload compared separately, since `Object`
  can fail where `Encode` does not; every accepted encoding fed back into the
  decode corpus.
- **2 494 graph cases** over one store holding a vendored tree (a commit in
  an `S_IFDIR` entry, with history), a DirNode-rooted directory with such an
  entry, a labelled conflict, a commit keyed by the old rule and one off by
  one, a child of the old-rule commit, junk under a Commit key, a DirNode
  whose child is a commit, a regular-file entry holding a commit, a commit
  whose tree is absent, an absent commit, and an unkeyable footprint:
  `child_keys`, `dir_of`, `collect_entries`, `list_entries` (4 limits × 4
  cursors), `lookup_entry` (15 names), `resolve_path` and `resolve_entry` (25
  paths), `reachable_keys` and `check_complete` (order included),
  `tarexport::write` (archive bytes), `put_verified`, from 28 roots; plus
  `put_verified` and `child_keys` at eight lengths around a conflicted
  commit's footprint, a wrong hash, and 15 `footprint` sums.
- **The CLIs, side by side**: both binaries over copies of one store built
  through the library (conflicted and nameless commits, the vendored tree, the
  old-rule commit, junk, the odd entry): 60 commands (`commit show`, `ls
  --keys`, `export`, `ref set`, `commit create` with good and bad change ids,
  trees and parents). Identical stdout, archives and exit codes, and identical
  error text, in all 60.

The error strings pinned in `decode_rejects_jj_fields`, the two new
`decode_rejects` cases and `encode_rejects_invalid_jj_fields` were checked
against the oracle's verdicts for the same inputs (32 of them byte-identical
"pin" cases added to the corpus under the tests' names), not predicted.

### Tests

| Go | Rust |
|----|------|
| `commit/commit_test.go` (new and changed cases) | `src/commit.rs`: `conflicted_commit_matches_hand_assembled_bytes`, `new_fields_round_trip`, `encode_accepts_jj_bounds`, `encode_rejects_invalid_jj_fields`, `decode_rejects_jj_fields`, `signature_payload_covers_new_fields`, `object_length_is_the_footprint`, `object_rejects_length_overflow`, `footprint_sums_own_bytes_and_trees`, `golden_vector_conflicted`; `object_key`, `decode_rejects` and the first golden vector updated |
| `fstree/commit_test.go`, `fstree/commit_keyed_test.go` | `src/fstree/read.rs`: `child_keys_conflicted_commit`, `reachability_covers_every_conflict_term`, `dir_of_cases`, `readers_pass_through_a_commit`, `dir_leaf_length_counts_a_commit_entry`, `check_complete_through_a_commit_entry`, `commit_keyed_without_its_footprint_is_rejected`, `commit_as_a_dir_node_child_is_rejected`, plus `commit_whose_footprint_overflows_is_rejected` |
| `TestVerifyObjectChecksCommitLength` (rewritten) | `src/packstore/verify.rs` |
| `TestTreeHoldingACommitKeepsItsHistoryLive` | `src/gc/tests.rs` |
| `TestWrite_ReadsThroughACommit` | `src/tarexport.rs` |
| `TestE2E_Commit` (change id), `TestE2E_CommitInsideADirectory`, `TestE2E_CommitKeyedByTheOldRuleIsRefused`, `TestE2E_CommitUnderARegularFileEntryIsRefused`, `TestRenderCommitConflicted`, `TestIdentityLineWithoutNameOrEmail` | `tests/cli_e2e.rs` |
| — | `tests/golden_commit.rs` over 13 Go-generated cases: footprint, children and the old-rule refusal checked per case |

### Pins

Moved with this change: Go `v0.0.10` in `PORTING.md`'s first paragraph, the
"pinned at" line of `README.md`, `ref:` in `.github/workflows/ci.yml` and the
module version in `tools/vectorgen/go.mod` and `go.sum`; the crate version
for the API breaks above. `tools/vectorgen/commit.go` uses the new Go fields,
so it builds only against a Go module that contains PR #15. Against the
previous pin the interop job fails at its first commit comparison (`root
commit keys differ`): the key rule itself changed, so the two CLIs key the
same commit differently, long before the script reaches `--change-id`.
