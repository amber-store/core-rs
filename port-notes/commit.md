# commit — port notes

Backport of Go PR #12 (amber-store/core, merge commit `e318780`, tag
`v0.0.9`, 2026-09-23): the `Commit` object, CAS type 5. Port of Go
`commit/commit.go` plus the touch points in `key`, `fstree`, `packstore`
and the dev CLI. The format is specified in `architecture/commits.md`
(copied verbatim from the Go repository, like the rest of `architecture/`).

Public API (`src/commit.rs`): `Commit` and `Identity` (public fields),
`Commit::encode` / `object` / `signature_payload` / `decode`, the error enums
`Error`, `ConvertError`, `IdentityError`, `TextError`, and the constants
`MAX_PARENTS`, `MAX_IDENTITY_LEN`, `MAX_MESSAGE_LEN`, `MAX_SIGNATURE_LEN`,
`MAX_PUBLIC_KEY_LEN`, `MAX_TZ_OFFSET`.

| Go | Rust |
|----|------|
| `Commit{Tree, Parents, Author, Committer, Message, Signature, PublicKey}` | `Commit { tree, parents, author, committer, message, signature, public_key }` |
| `Identity{Name, Email, When, TZOffset int}` | `Identity { name, email, when, tz_offset: i32 }` |
| `(Commit).Encode` / `Object` / `SignaturePayload` | `encode` / `object -> (Key, Vec<u8>)` / `signature_payload` |
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
- **`signature`/`public_key`: empty = absent** (Go `omitempty`), as in
  `Reference`.
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
