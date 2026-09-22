# Port notes: `key` (Go `key/` → `src/key.rs`)

## API mapping

| Go | Rust |
|----|------|
| `key.Size` | `key::SIZE` (`usize`, module const) |
| `key.Type` (`uint8` alias) | `key::Type` enum, `#[repr(u8)]`, variants `Blob=0..XattrSet=4` |
| `Type.IsValid()` | `Type::is_valid(v: u8) -> bool` (associated fn on the raw value; see below) |
| — | `Type::from_u8(v: u8) -> Option<Type>` (new; needed because the enum cannot hold reserved values) |
| `Type.String()` | `impl Display for Type` |
| `key.Key` (`[32]byte`) | `key::Key(pub [u8; 32])` — `Copy + Eq + Ord + Hash`, public field for Go-like byte transparency (`k.0[31]`, `&k.0[..]`) |
| `k.Type()` | `k.type_()` |
| `k.LengthSize()` | `k.length_size()` (`usize`) |
| `k.Length()` | `k.length()` (`u64`) |
| `k.Hash()` | `k.hash()` (`&[u8]`) |
| `New` | `Key::new(t, length, serialized) -> Key` — **infallible** (see below) |
| `NewFromHash` | `Key::new_from_hash(t, length, [u8; 32]) -> Key` — **infallible** |
| `Parse` | `Key::parse(&[u8]) -> Result<Key, Error>` |
| `k.Validate()` | `k.validate() -> Result<(), Error>` |
| `k.String()` | `impl Display for Key` (lowercase hex); `Debug` prints `Key(<hex>)` |
| `lengthSizeFor` | `length_size_for` (private, as in Go) |
| sentinels in `errors.go` | `key::Error` enum (`thiserror`), `Copy + PartialEq` so callers match variants where Go uses `errors.Is` |

Extras: `Key::as_bytes(&self) -> &[u8; 32]` and `impl AsRef<[u8]>` (Go call
sites use `k[:]`; downstream modules — amberpack record bytes, packstore
fanout on the last key byte — need raw access).

## Decisions a reviewer should check

- **`Type` is a closed enum; Go's is an open `uint8`.** Consequences, all
  deliberate:
  - `New`/`NewFromHash` cannot receive a reserved type, so their only Go error
    path (`ErrReservedType`) is unrepresentable → `Key::new` /
    `Key::new_from_hash` return plain `Key`, not `Result`. Go's
    `TestNewFromHash_ReservedType` is therefore untestable as written; the
    reserved-type error is still produced and tested via `validate`/`parse`
    on raw bytes (`validate_reserved_type`).
  - `Type::is_valid` takes the raw `u8` (mirroring Go's ability to test
    `Type(255)`, ported in `type_is_valid`); on the enum itself it would be
    vacuously true.
  - Go's `Type(7).String() == "Type(7)"` fallback is unrepresentable. The
    numeric form survives in `Error::ReservedType(u8)`, whose Display
    (`"key: reserved object type: 7"`) matches Go's wrapped
    `fmt.Errorf("%w: %d", ...)` text exactly. Noted in `type_display` test.
- **`Key.0` is public** (like Go's transparent `[32]byte`), so non-canonical
  keys are constructible without `parse`. Accessors document the same contract
  as Go ("assume the key is canonical"). The one place the contract bites:
  `type_()` on a reserved nibble **panics** (Go returned `Type(7)` and let
  dispatch `default:` arms handle it). Rationale: untrusted bytes must go
  through `Key::parse` (every Go data path does the equivalent), so the panic
  is a caller-bug assertion, not a data-path panic; returning `Option<Type>`
  would force `unwrap` litter on every canonical-key dispatch downstream.
  Downstream ports of Go `default:` arms ("unknown object type") can treat
  those arms as unreachable *after validation*. Covered by
  `type_accessor_panics_on_reserved_nibble`.
- **Validation order** ported exactly: reserved bit → type valid → canonical
  length (extra test `validate_order_reserved_bit_before_type` pins bit vs.
  type precedence). The canonical-length condition is Go's verbatim:
  `k[1] == 0 && !(length_size() == 1 && length() == 0)`.
- **Error messages** byte-match Go's (`error_messages_match_go`), including
  `Parse`'s `": got %d"` detail carried as `Error::BadKeyLength(usize)`.
- `length_size_for`: minimal big-endian byte count via
  `(u64::BITS - leading_zeros).div_ceil(8)` ≡ Go's `(bits.Len64(l)+7)/8`;
  zero → 1 (the single `0x00` byte). Boundary cases 1/255/256/…/u64::MAX
  ported in `new_from_hash_length_size_boundaries`.
- BLAKE3: Go `zeebo/blake3.Sum256` ↔ Rust `blake3::hash`; pinned by the
  empty-input known-answer test (`af1349b9…`) ported from Go.

## Go quirks preserved

- The all-zero key is canonical (`Blob`, length 0, zero hash bytes) —
  `validate_zero_length_is_canonical`.
- `length` is logical, taken verbatim, never checked against
  `serialized.len()` — `new_length_is_logical_not_serialized`.
- Derived `Ord` on `Key([u8; 32])` is byte-lexicographic, matching Go
  `bytes.Compare` usage downstream.

## Tests

- Unit (`src/key.rs`): all meaningful Go tests from `key_test.go` +
  `type_test.go` ported (20 tests), plus Rust-specific pins (validate order,
  error text, `type_` panic).
- Golden (`tests/golden_key.rs`): `keys.json` per VECTORS.md — recompute
  `Key::new(type, length, payload)`, compare hex, round-trip through
  `Key::parse`, check accessors and hash truncation. `length` parsed with
  `common::parse_u64` (decimal string, may exceed 2^53). Vectors were **not
  yet generated** at port time: verified to skip under
  `AMBER_GOLDEN_OPTIONAL=1` and to fail loudly without it. **TODO: re-run
  without the env var once `tests/golden/keys.json` lands.**

## Adversarial review (2026-08-08)

Line-by-line comparison of `src/key.rs` against Go `key/{key.go,type.go,errors.go}`
and both Go test files. **No semantic defects found**: header-byte packing,
big-endian length encoding/decoding, `lengthSizeFor` (incl. zero → 1 and
`u64::MAX` → 8), hash truncation `32 - 1 - ls`, validation order
(reserved bit → type → canonical length, condition verbatim
`k[1] == 0 && !(ls == 1 && length == 0)`), Parse's length-check-first order,
and all four error message texts match Go exactly. No panic is reachable from
untrusted input: `parse` bounds-checks before copying, and all accessor
indexing is within `1 + 8 ≤ 32`. The `type_()` panic on a reserved nibble is
reachable only by hand-constructing a `Key` via the public field, bypassing
`parse`/`validate` — reviewed and accepted as documented above.

Review additions (tests only, no implementation changes):

- `ord_is_byte_lexicographic` — pins derived `Ord` ≡ raw byte order
  (Go `bytes.Compare` parity that packstore/fstree will rely on; previously
  claimed in these notes but untested).
- `debug_and_byte_accessors` — pins `Debug` = `Key(<hex>)` and
  `as_bytes`/`AsRef` consistency.

**Golden vectors landed during review** (`tests/golden/keys.json`, 15 cases:
all 5 types, length-field sizes 1–8, length 0, `u64::MAX` — a decimal string
> 2^53 exercising `common::parse_u64` — and 8 cases with logical length ≠
payload length). `cargo test --test golden_key` passes **without**
`AMBER_GOLDEN_OPTIONAL`; the hand-off TODO is resolved.

Gates at review close: full `cargo test` (no env var) green — 74 unit + all
integration suites. `rustfmt --check` clean on `src/key.rs` and
`tests/golden_key.rs`. Whole-crate `cargo fmt --check` and `cargo clippy
--all-targets -- -D warnings` still fail **only** in other agents' files
(`src/amberignore.rs` ×2 collapsible_match + fmt; `tests/common/mod.rs` fmt) —
outside this module's write scope, owners notified via review report.

## Build status at hand-off

On the live tree: `cargo clippy --all-targets -- -D warnings` passes;
`AMBER_GOLDEN_OPTIONAL=1 cargo test key` passes (20 unit + 1 golden from this
module; the filter also catches a few other modules' tests, all green);
without the env var the golden test fails loudly on the missing vectors, as
required. `rustfmt --check` is clean on `src/key.rs` and
`tests/golden_key.rs`; full `cargo fmt --check` fails only on
`tests/common/mod.rs` (shared helper, not touchable from this task): rustfmt
wants the `panic!` at line ~68 wrapped. Whoever owns the shared helpers
should run `cargo fmt` on it — no semantic change needed.

## Go PR #12 backport (2026-09-23)

`Type::Commit = 5` (Go `key.Commit`); the reserved range is now 6..=15.
`is_valid`, `from_u8` and `Display` follow; the tests that used 5 as "a
reserved type" use 6. A `Commit` key's length field is the object's own
serialized byte length, the `Blob`/`XattrSet` rule. `keys.json` needed no
regeneration: it never pinned 5 as reserved. See `port-notes/commit.md`.
