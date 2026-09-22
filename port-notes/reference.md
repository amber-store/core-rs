# reference — port notes

Port of Go `reference/reference.go` (the named-pointer record: canonical CBOR
codec + validation). Public API: `Reference` struct (public fields),
`Reference::encode` / `Reference::decode` / `Reference::signature_payload`,
free `validate_name` / `validate_user`, constants `MAX_NAME_LEN`,
`MAX_USER_LEN`, `MAX_SIGNATURE_LEN`, `MAX_PUBLIC_KEY_LEN`.

## Modeling decisions

- **`signature`/`public_key` are `Vec<u8>`, empty = absent.** Go's
  `omitempty` omits both nil and zero-length slices, so a present-but-empty
  key 4/5 is unrepresentable there too; a plain `Vec` mirrors that exactly
  (no `Option` ambiguity).
- **`name`/`user` are `String`,** so Go's "must be valid UTF-8" branches are
  unreachable through `validate_name`/`validate_user`. The `NotUtf8` error
  variants are kept (with the verbatim Go messages) for documentation; on the
  decode path invalid UTF-8 in a text string is rejected during
  unmarshalling (`DecodeError::InvalidUtf8`), which is where Go's fxamacker
  rejects it as well.
- Errors are one `Error` enum whose `Display` strings reproduce the Go
  diagnostics verbatim (`"reference name must not be empty"`,
  `"reference key: %w"`, `"invalid reference: %w"`, ...). `encode` returns
  validation errors bare (as Go's `Encode` does); `decode` wraps the same
  failures in `Error::Invalid`, and decode-stage failures in `Error::Decode`.

## Decode: exact port of the Go enforcement

Go's `Decode` is lax-`cbor.Unmarshal` → `validate()` → re-encode with the
deterministic mode → byte-compare. The re-encode comparison is ported as-is,
so the acceptance set is *exactly* "canonical encodings of valid records" on
both sides.

The lax unmarshal is hand-rolled to fxamacker v2.9.2's observable behavior
(the version the Go module pins), verified differentially (see below):

- **Two passes, like fxamacker:** a whole-document well-formedness check
  (nesting cap included) plus extraneous-trailing-data rejection runs
  *before* any field decoding. Ordering is observable: `{0: 5}‖junk` reports
  extraneous data, not the field-type error; `{4: <32 nested arrays>}`
  reports the nesting error, not the type error.
- **Nesting cap** (`MaxNestedLevels` = 32): depth starts at 0; arrays/maps
  increment-then-check; in a tag chain the first tag adds no level, each
  additional one does (scanned iteratively, per fxamacker's
  `wellformedInternal`). Verified boundaries: under the top-level map,
  31 nested arrays pass / 32 fail; 32 nested tags pass / 33 fail.
- **Map keys:** unsigned-int keys 0–5 match fields; other integer and
  text-string keys are skipped quietly; byte-string / array / map / tag /
  primitive keys are an unmarshal error (`DecodeError::BadMapKeyType`,
  fxamacker: "map key is of type X and cannot be used to match struct field
  name"). fxamacker *parses* integer and text keys before matching, so an
  integer key whose value overflows int64 (`DecodeError::IntKeyOverflow`,
  message verbatim) and a text key with invalid UTF-8
  (`DecodeError::InvalidUtf8`) are unmarshal errors even though such keys can
  never match a field.
- **Simple values:** two-byte simple values < 32 (`0xF8 0x00..=0x1F`) are a
  well-formedness error anywhere in the document
  (`DecodeError::InvalidSimpleValue`, fxamacker message verbatim). Unassigned
  simple values (0–19, 32–255) *fill integer targets numerically* in
  fxamacker (`fillPositiveInt`), so `{3: simple(42)}` decodes
  `created_at = 42` and is then rejected as non-canonical; into string/bytes
  fields they remain type errors, and bool stays a type error everywhere.
- **Tags:** mirrored from fxamacker's `parseToValue`, at the top level and on
  matched field values (never on skipped items): leading self-described-CBOR
  tags (55799) are stripped; every tag number in the remaining chain is
  validated against its immediate content type for built-in tags 0–3
  (`DecodeError::BadTagContent`, messages verbatim); bignum tags 2/3 follow
  the big.Int paths — into `created_at` they yield the integer value (or
  `DecodeError::BignumOverflow`), into the byte-vector fields they yield the
  *raw content bytes*, into strings and the top-level record they are type
  errors; all other tags (0, 1, 4, 21–23, ...) are unwrapped transparently
  and their content decoded. A tag-wrapped canonical map therefore decodes
  and is rejected as "not canonical", exactly like Go.
- **Arrays into byte-vector fields:** fxamacker decodes a CBOR array into
  `[]byte` element-wise (like `encoding/json`): unsigned integers and simple
  values ≤ 255 fill bytes, null/undefined become 0, bignum tag 2 in range
  fills, everything else is a type error (`DecodeError::ByteElemType`) or an
  overflow (`DecodeError::ByteElemOverflow`).
- **Duplicate keys:** first value wins; the duplicate entry's value is
  skipped *without type-checking* (fxamacker `DupMapKeyQuiet`) — e.g.
  `{2:"u", 2:<uint>}` is not a type error.
- **Null/undefined** (`0xF6`/`0xF7`), top-level or as a field value, decode
  to the zero value, as fxamacker does.
- **Non-shortest heads** are accepted here (then caught by the canonical
  comparison), matching both fxamacker and this crate's `cbor::read_head`.
- `created_at` accepts uint/negint within `i64`; out-of-range is an
  unmarshal error (`IntOverflow`).

## Documented divergences (all rejection-outcome-identical)

1. **Indefinite-length / reserved heads (additional info 28–31):** rejected
   at the shared head reader with a decode-stage error; Go's fxamacker parses
   indefinite items and rejects the record later ("invalid reference" or
   "not canonical" depending on content). The outcome is provably identical:
   Go can only *accept* input equal to the canonical re-encoding, which never
   contains such heads.
2. **Decode-stage message text** approximates fxamacker's wording (typed
   variants carry the same facts: offending CBOR type, field, offsets).
   Validation-stage messages match Go byte-for-byte. The extraneous-data
   message reproduces fxamacker's format exactly
   (`"cbor: N bytes of extraneous data starting at index I"`).
3. **fxamacker's `maxArrayElements`/`maxMapPairs` caps (131072)** on claimed
   definite lengths are not replicated; an overlong claim fails with
   `UnexpectedEof` when the elements run out instead (same decode-stage
   classification, no allocation either way).
4. Go's defensive `"re-encoding reference: %w"` wrap is unreachable there and
   has no counterpart here (the Rust encoder is infallible).

## Differential verification (harness deleted afterwards)

A throwaway Go oracle (module in /tmp requiring the pinned
`amber-store-core` commit) generated:

- **129 encode cases** (unicode/boundary names and users, negative and
  extreme `created_at`, signature/public-key sizes at and past every bound,
  malformed keys): Rust encodings byte-identical, `signature_payload`
  byte-identical, error strings verbatim-identical.
- **1294 decode cases** (all valid encodings above; two mutation sweeps —
  truncations at every length and five byte-mutations at every offset of a
  signed and an unsigned record; hand-crafted shapes; splitmix garbage):
  accept/reject identical on every case; error classification identical
  except the documented indefinite-head class; every
  `"invalid reference: ..."` message verbatim-identical.
- Targeted probes for map-key types, duplicate-key semantics, nesting
  boundaries (including 10^6 nested tags — no stack overflow), and
  well-formedness ordering; all pinned as unit tests.

## Adversarial review (2026-08-08)

A second differential pass (side-by-side read of fxamacker v2.9.2's
`valid.go`/`decode.go` plus a fresh Go oracle: **21197 cases** — the field ×
value matrix over every CBOR shape incl. tags/bignums/simple values, map-key
shapes, duplicate keys, 3000 random structured items in value/key/top-level
positions, full-offset single-byte substitution and sequence-insertion
sweeps, 8000 double mutations, truncations, deterministic garbage) found
**five error-classification drifts**, all on rejected inputs (the accept set
was never affected — acceptance requires byte-equality with the canonical
re-encoding on both sides). All five are fixed and covered by unit tests:

1. Two-byte simple values < 32 were accepted by the well-formedness pass
   (fxamacker rejects, RFC 8949 §3.3) → now `InvalidSimpleValue`.
2. Integer map keys overflowing int64 were skipped quietly (fxamacker
   errors) → now `IntKeyOverflow`.
3. Text map keys with invalid UTF-8 were skipped quietly (fxamacker parses
   keys before matching, errors) → now `InvalidUtf8`.
4. Tags on field values / the top level were a flat type error (fxamacker
   strips 55799, validates built-in tags 0–3, decodes bignums, unwraps other
   tags — often yielding a "not canonical" rejection instead) → now mirrored
   (`BadTagContent`, `BignumOverflow`, transparent unwrap).
5. Unassigned simple values into `created_at` and CBOR arrays into the
   byte-vector fields were flat type errors (fxamacker fills them) → now
   mirrored (`ByteElemType`, `ByteElemOverflow`).

After the fixes the full 21197-case oracle matches with **zero mismatches**
(same verdict class, verbatim-identical `"invalid reference: ..."` messages,
field-identical accepts), the sole exception remaining the documented
indefinite/reserved-head class below.

## Test key note

The Go tests derive their valid key via `fstree.EncodeBlob("hello")`; the
Rust unit tests use `key::Key::new(Type::Blob, 5, b"hello")` to avoid a
cross-module dependency. Equivalence is pinned by the ported `golden_vector`
test (byte-for-byte against the Go suite's `goldenHex`).

## Go PR #12 backport (2026-09-23)

The private `append_tstr` / `append_int` encoders moved to `cbor.rs` as public
helpers, shared with the new `commit` codec. No behavior change; the unit
tests and `reference.json` pin the encoding as before.
