# tar (tarexport + tarextract) port notes

Files: `src/tarexport.rs`, `src/tarextract.rs`; tests `tests/golden_tar.rs`,
`tests/tar_extract.rs`. Go references: `tarexport/tarexport.go`,
`tarextract/tarextract.go`, and the **Go 1.26 `archive/tar`** sources
(`writer.go`, `common.go`, `format.go`, `strconv.go`, `reader.go`), which the
Go side delegates to and which had to be ported for byte identity.

## Layout decision: shared framing lives in tarexport.rs

The low-level tar machinery is `pub(crate)` in `src/tarexport.rs` and reused
by `src/tarextract.rs` (not duplicated): block-layout constants, typeflag and
format-bit constants, `compute_checksum`, `block_padding`, `is_header_only`,
`PaxTime`, `TarHeader`, `PaxRecords`, `valid_pax_record`,
`format_pax_time/record`, `path_clean/join/split`, `is_ascii_str`/`to_ascii`,
`GoQuote`, and the writer (`TarWriter`). The reader (`TarReader`, parsers,
`merge_pax`) lives in tarextract.rs, `pub(crate)` so tarexport's unit tests
can read back their own output.

## Writer: ported subset of Go archive/tar

Ported exactly (all decisions taken by Go on the tarexport path):

- `Header.allowedFormats`: `verifyString`/`verifyNumeric`/`verifyTime` in Go's
  field order, `fitsInOctal`/`fitsInBase256`, `preferPAX`, the
  `PAXRecords`-copy rules (`basicKeys`, `GNU.sparse.` exclusion), record
  validation, and the desired-format mask (`PAX ⇒ USTAR also allowed unless
  preferPAX`). GNU exclusion is tracked bit-for-bit so the USTAR/PAX
  selection and error reasons match, even though the GNU *writer* is not
  ported.
- `writeUSTARHeader` (incl. `splitUSTARPath`), `writePAXHeader`
  (`PaxHeaders.0` naming via `path.Split`+`path.Join`, record sorting =
  bytewise key order via `BTreeMap`, `maxSpecialFileSize` cap),
  `writeRawFile` (toASCII, truncate at 100, `TrimRight "/"`, octal-zero
  fields), `writeRawHeader` (header-only size zeroing, padding bookkeeping),
  `templateV7Plus`, `Flush`, `Close` (two zero blocks), `regFileWriter`
  semantics on the `io::Write` impl (`archive/tar: write too long`).
- `formatString` (incl. the buggy-reader trailing-slash fix-up),
  `formatOctal` (zero-on-overflow with sticky error, ignored on the PAX main
  header exactly as Go ignores it), `formatPAXRecord` (self-including length
  with the one-round adjustment), `formatPAXTime` (negative-time correction,
  trailing-zero trim), `validPAXRecord`, `toASCII`/`isASCII`.
- Go `path.Clean`/`Join`/`Split` ported bytewise (`Join` keeps later empty
  elements as separators — that is what turns `("sub/", "PaxHeaders.0", "")`
  into `sub/PaxHeaders.0` for directory entries).
- `time.Unix` normalization (`PaxTime::unix`), `Time.UnixNano` wrapping.

`toASCII` note: Go iterates runes and drops runes ≥ U+0080 plus NULs; since
every byte of a multi-byte or invalid UTF-8 sequence is ≥ 0x80, this is
equivalent to the byte filter implemented here (keep `0x01..=0x7F`).

Deliberately not ported (unreachable from tarexport, which pins
`Format=PAX`):

- The GNU writer (`writeGNUHeader`, `formatNumeric` base-256 encoding). If a
  header were only GNU-encodable the writer returns a header error instead.
- Sparse files (disabled in Go too), `TypeXGlobalHeader`'s
  `reflect.DeepEqual` field guard (the `mayOnlyBe(PAX)` effect *is* ported),
  the write-after-close latch (the Rust writer is single-shot), and
  `AddFS`/`readFrom`.
- The `Format==FormatUnknown` ModTime rounding exists but is simplified to
  half-up on the sub-second part (exact for all Unix-era times; tarexport
  never takes this branch).

Error-text deviations (error paths only, never bytes): `verifyTime` messages
render the timestamp in PAX form where Go uses `%v` of `time.Time`; `GoQuote`
reproduces Go `%q` exactly for ASCII and approximately for non-ASCII
(printable Unicode kept, invalid bytes as `\xNN`, no `\u` escapes).

## Exporter (tarexport.go proper)

Direct port: root must be DirLeaf/DirNode; `collect_entries` per directory;
unsafe-name refusal (`""`, `.`, `..`, `/`); `path.Join` prefixing; `mode &
0o7777`; `time.Unix(0, mtime)`; xattrs (inline or spilled, wrong-size
`xattrs_key` silently ignored exactly like Go's `default:` arm) into
`SCHILY.xattr.*` records; dir trailing slash + recursion after the header;
regular-file size from the content key's length field; symlink/fifo/devices
(`rdev` pair required); sockets skipped; unsupported types rejected with Go's
`%#o` rendering. Uid/gid/devmajor/devminor are `u64 → i64` casts, matching
Go's `int(...)`/`int64(...)` conversions.

Error enum mirrors Go's wrap sites and texts; `Walk`, `Key`, and
`XattrsDecode` are returned unwrapped as Go returns them, `Content` carries
the `tarexport: %w` prefix.

## Reader (tarextract): ported subset of Go archive/tar reader

Ported: two-zero-block termination (including Go's quirks: clean EOF at a
block boundary or inside padding ends the archive; zero block + non-zero
block is `ErrHeader`), checksum verification (unsigned + signed), magic
sniffing (USTAR/PAX, STAR, GNU, V7), V7+USTAR field parsing (`parseString`,
`parseOctal` trimming, base-256 `parseNumeric` with overflow checks), USTAR
prefix joining, STAR atime/ctime + 131-byte prefix, the GNU pre-1.8
skeptical atime/ctime fallback, PAX extended headers (`parsePAX`,
`parsePAXRecord`, `parsePAXTime` with digit truncation and negative
correction, `readSpecialFile` 1 MiB cap), `mergePAX` (empty values keep the
USTAR value; `uid`/`gid`/`size` re-parse errors are `ErrHeader`), TypeRegA
promotion, size re-setup after merge, and global headers ('g') surfacing as
entries (which `extract` then rejects like Go does).

Not ported (out of scope for what tarexport emits; foreign archives only):

- GNU long-name/long-link meta entries ('L'/'K') — they surface as entries
  and `extract` fails with `unsupported tar type 'L'` instead of being
  spliced into the next header.
- Sparse files in all encodings (old GNU, PAX 0.x/1.0); GNU sparse PAX
  records are carried verbatim but not interpreted, and the parsePAX
  0.0→0.1 sparse-map transformation is skipped.
- `Header.Format` guessing refinements (ASCII/NUL-termination strictness) —
  the guess is not carried at all; extract never consults it.
- `filepath.IsLocal`/`tarinsecurepath` (Go gates it behind GODEBUG;
  tarextract has its own stricter `safeJoin`).

## Extract (tarextract.go proper)

Direct port of the restore policy: creation in archive order; directories
`MkdirAll(0o700)` with metadata (mode, ownership, xattrs, mtime) deferred to
after all members; regular files `O_WRONLY|O_CREATE|O_EXCL, 0o600` then
`applyMeta`; symlinks (no chmod, no xattrs, `AT_SYMLINK_NOFOLLOW` mtime);
fifos via `mkfifo(mode&0o7777)`; devices via `mknod` where a privilege error
(EPERM/EACCES/EOPNOTSUPP) prints the `amber-store: skipping device node`
warning and continues, any other error is fatal; xattrs via `lsetxattr`
(darwin: `setxattr` + `XATTR_NOFOLLOW`) where privilege errors or ENOTSUP
warn and continue, others are fatal; ownership only when euid == 0; mtime
set last via `utimensat` with `NsecToTimespec` semantics.

`safeJoin` quirks ported verbatim: `..` components are refused anywhere
(even resolvable ones), absolute names are re-rooted under dest rather than
rejected, and the prefix check compares against `dest` exactly as given (so
a trailing-slash dest refuses everything, like Go).

Small deviations: `write_regular` cannot observe the `close(2)` error
(Rust's `File` drop; Go checks `f.Close()`); the stderr warnings print Rust's
`io::Error` Display ("Operation not permitted (os error 1)") where Go prints
the bare errno text; `dev_t` packing (`mkdev`) is hand-written per OS to
match `unix.Mkdev` (macOS `major<<24|minor`, Linux glibc packing; other
unixes fall back to the Linux formula).

## Verification

- `tests/golden_tar.rs`: the exporter's output over the golden fstree
  (objects.bin + manifest root) is byte-identical to `tar_go.tar`
  (11,704,832 bytes). Passed on first run.
- Differential testing against Go (per PORTING's proven technique): a
  throwaway Go harness (stdlib `archive/tar`, headers shaped exactly as
  tarexport shapes them) generated 19 oracle streams — plain USTAR, long
  splittable names with and without ns-mtime, a 150-byte single-component
  name, non-ASCII names and linknames, 150-byte linkname, char device with
  uid/gid 4294967294 and devmajor 259, mtime 2^33 s, negative mtime, xattr
  record sets (binary + empty values, sorting around `mtime`),
  `PaxHeaders.0` naming for dirs (incl. the `sub/PaxHeaders.0` empty-file
  case, >100-byte truncation, and the trailing-slash fix-up at byte 100),
  9 GiB size record, uid octal boundary 2097151/2097152, and names of
  exactly 100/101 bytes. The Rust writer matched all 19 streams
  byte-for-byte. The harness and its temporary driver test were deleted
  afterwards as instructed.
- `tests/tar_extract.rs`: extracts the golden tar and verifies the
  unprivileged subset (file contents incl. `big.bin` == `data(3, 5242880)`,
  3000 bigdir entries with exact ns mtimes, symlink target + ns mtime, fifo,
  negative mtime on `sub/old`, setuid mode, xattrs where the platform
  allows, devices skipped as non-root / present as root, socket absent),
  plus a public-API export→extract round trip. Go's `tarextract` tests
  (deferred read-only dir metadata, unsafe-name rejection) are ported as
  unit tests in `src/tarextract.rs`; Go's `tarexport` tests as unit tests in
  `src/tarexport.rs`. Reader/writer primitives carry the Go
  `strconv_test.go` vectors (`formatPAXTime`, `formatPAXRecord`,
  `parsePAXTime`, `parsePAXRecord`, `parseNumeric`) and `path.Clean` /
  `splitUSTARPath` tables.

## Status of shared gates

`cargo test` is green for the whole crate. `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` are clean for the four tar
files; at the time of writing, crate-wide clippy still fails on
`src/inbox.rs` (another agent's in-progress module, two
`manual_range_patterns` findings) — not touched per the module-ownership
rules.

## Go PR #3 backport (2026-08-27)

- `extract` now calls `reject_symlink_components` (an Lstat walk of the
  target's components below dest) after the lexical `safe_join`, refusing to
  extract through any existing non-directory — the symlink write-through
  hole reproduced in this port before the fix. New error variant
  `Error::ThroughNonDirectory`, same message as Go.
- `apply_meta` chowns before chmodding, since chown(2) clears
  setuid/setgid. The ported setuid test, like Go's, only exercises the
  path as root (it returns early otherwise, Rust's nearest t.Skip).

## Go PR #15 backport (2026-09-23)

`write` accepts a Commit root, which stands for its tree, and a commit held
by an `S_IFDIR` entry exports as a plain directory: both come from
`fstree::collect_entries`, which takes a commit for the directory it records.
The commit objects themselves never reach the archive. Port of Go
`TestWrite_ReadsThroughACommit`; the differential run compared archive bytes
for 28 roots (`port-notes/commit.md`).
