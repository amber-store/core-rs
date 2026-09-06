# amberpack port notes

Source: `amberpack/record.go`, `amberpack/pack.go` at the pinned commit; tests
ported from `record_test.go` + `pack_test.go`.

## API mapping

| Go | Rust |
|----|------|
| `RecHeaderSize` | `REC_HEADER_SIZE` |
| `EncodeRecord(k, data)` | `encode_record(k, &data)` |
| `ParseRecord(b)` / `Record{Key,Flags,Ulen,Slen}` | `parse_record(&b)` / `Record{key,flags,ulen,slen}` |
| `DecodePayload(flags, ulen, stored)` | `decode_payload(flags, ulen, &stored)` |
| `Writer.Add(fstree.Object)` | `Writer::add(key, &bytes)` |
| `Writer.AddRecord(rec)` | `Writer::add_record(&rec)` |
| `Writer.Close()` | `Writer::finish() -> Result<W, _>` (returns the inner writer; does not close the destination, same as Go) |
| `Reader.All()` (`iter.Seq2`) | `Reader` implements `Iterator<Item = Result<(Key, Vec<u8>), Error>>` |
| `errors.Is(err, ErrCorrupt/ErrMalformed)` | `Error::is_corrupt()` / `Error::is_malformed()` |

`fstree` is still a placeholder in this crate, so the wire-pack API takes
`(Key, &[u8])` / yields `(Key, Vec<u8>)` instead of a `fstree::Object` struct.
When fstree lands, `Writer::add(o.key, &o.bytes)` is the one-line adapter; no
semantic difference. (Go's `mkObj`/`fstree.EncodeBlob` in the tests is
replaced by `Key::new(Type::Blob, len, data)` — a Blob's serialized bytes are
the raw data.)

## Error classes

Typed enum with four variants:

- `Corrupt(String)` — Go `fmt.Errorf("%w: …", ErrCorrupt)`. Display is
  byte-identical to Go: `amberpack: corrupt pack data: <detail>`.
- `Malformed(String)` — Go `ErrMalformed` wrapper:
  `amberpack: malformed pack stream: <detail>`.
- `TooLarge{key, len}` — Go's plain (non-sentinel) EncodeRecord error
  `amberpack: object <hex> too large: <n> bytes`.
- `Io` — writer-side passthrough. The **Reader never returns `Io`**: exactly
  like Go, every stream read failure is classified `Malformed`.

Go subtlety preserved: the Reader wraps a corrupt record as
`fmt.Errorf("%w: %v", ErrMalformed, err)` — `errors.Is(…, ErrCorrupt)` is
**false** on reader errors. The port does the same: reader errors are
`Malformed` with the corrupt text embedded in the message
(`amberpack: malformed pack stream: amberpack: corrupt pack data: record CRC
mismatch`); `is_corrupt()` returns false. Unit test
`reader_record_crc_mismatch` pins this.

Legacy versions `AMBERPK\x01` / `\x02` are rejected through the same
magic-comparison path as Go, with Go's exact diagnostic (`bad magic`), not a
dedicated "legacy" message — Go has none. `reader_rejects_legacy_versions`
pins the behavior for both bytes.

I/O error *text* inside Malformed messages differs from Go where the detail is
the platform error string (Go `unexpected EOF` vs Rust `failed to fill whole
buffer`); the surrounding context strings (`reading magic: …`,
`truncated record header: …`, …) match Go's.

## zstd

- Level: `zstd::DEFAULT_COMPRESSION_LEVEL` (= 3, the libzstd default), per
  PORTING.md. **Compressed bytes differ from Go** (klauspost) while remaining
  mutually decodable; raw records and all headers are byte-identical. The
  compress-only-if-strictly-smaller rule is identical, and both encoders agree
  on which golden payloads compress (all `records_compressed.json` cases get
  flag 1 from the Rust encoder too — asserted, not byte-compared).
- A `zstd::bulk::compress` failure (allocation-level; practically impossible)
  falls back to storing raw — indistinguishable from "did not get smaller".
  Go's `EncodeAll` cannot fail.
- `decode_payload` bounds the decompression buffer at `ulen`
  (`zstd::bulk::decompress(stored, ulen)`), so a frame expanding past the
  header's claim fails inside zstd (→ `Corrupt("zstd: …")`) instead of
  allocating first and then reporting Go's
  `decompressed to N bytes, header says M` (which the port still emits when
  the frame decodes to *fewer* than `ulen` bytes — including the empty-input
  edge, `decompressed to 0 bytes, header says N`, byte-identical to Go and
  pinned in `decode_payload_errors`). Same error class either way; deliberate
  hardening — Go (klauspost, default 64 GiB decoder cap) will materialize an
  over-expanding frame before rejecting it.
- Inside `Corrupt("zstd: …")`, the *library* error text differs between
  implementations for invalid frames (klauspost `invalid input: magic number
  mismatch` vs libzstd `Unknown frame descriptor`; over-expansion: Go's
  `decompressed to N bytes, header says M` vs libzstd `Destination buffer is
  too small`). Class and the `zstd: ` context are identical.
- Drop nuance: Rust's `BufWriter` flushes buffered bytes when an abandoned
  `Writer` is dropped; Go's `bufio.Writer` discards them. Either way an
  unfinished pack lacks the end marker and is invalid, so no reader-visible
  difference.

## Differential testing (performed, harness deleted)

Throwaway harnesses (`/tmp/amberpack-diff/{rs,go}`, Go side pinned to
e4fcb60… via a `replace` to the read-only checkout) verified, over 13 shared
deterministic payloads (incompressible splitmix, compressible const runs
1 B–200 KB, mixed):

- Go `ParseRecord`+`DecodePayload` accepts every Rust-encoded record,
  including 3 libzstd-compressed ones, with matching keys/payloads.
- Go `Reader.All` fully decodes a Rust-written wire pack; Rust fully decodes a
  Go-written pack (klauspost frames), sequence-equal.
- 7 tamper cases (truncated header, bad tag, bad flags, CRC flip, ulen+1,
  reserved key type, slen flip) produce **byte-identical error strings** in
  both implementations.

An independent adversarial-review harness (also deleted) re-verified the
above with 8 payloads and 22 tamper/edge cases, adding: flags `0x03`
(zstd + unknown bit), compressed `slen=0/ulen>0` records (both sides parse
them, then reject in decode with the identical `decompressed to 0 bytes…`
message), under-expanding frames (identical message), over-expanding frames
(identical class, divergent text per the note above), the oversized-`slen`
stream guard (identical message), and full cross-decoding of each side's
wire pack (identical key/len/CRC sequences, both directions). Error *classes*
matched in every case.

## Notes / quirks ported

- `parse_record` accepts trailing bytes past the record (mmap/tail-scan
  invariant) and never mutates its input; CRC is computed as
  `crc(b[..42]) ⊕ crc(zero4) ⊕ crc(payload)` exactly like Go.
- Validation order is Go's: header length → tag → flags → payload length →
  raw/compressed invariants → CRC → key canonicality.
- Raw `decode_payload` ignores `ulen` (Go behavior; `parse_record` enforces
  `ulen == slen` for raw records) and returns an owned copy.
- `payload_fits` bound (`<= u32::MAX`) kept as a separate testable function.
- The reader's 256 MiB `slen` guard fires after the 46-byte header read and
  before the payload allocation/read, as in Go.
- Writer writes the magic lazily on first `add`/`add_record` and on
  `finish` for an empty pack; `finish` writes exactly one `0x00` end marker
  and flushes. The reader stops at the end marker without touching trailing
  bytes (Go behavior).
- `Reader` is fused: it yields at most one `Err` and then `None`, mirroring
  "All yields exactly one error (and stops)". Go's "All must be called at
  most once" footgun is unrepresentable (the iterator owns the stream
  position).

## Go PR #3 backport (2026-08-27)

- `MAX_PAYLOAD` is now `pub` and set to 256 MiB, replacing both the old
  private 4 GiB `MAX_PAYLOAD` and `MAX_WIRE_PAYLOAD`; `parse_record` rejects
  `ulen > MAX_PAYLOAD` (between the raw ulen/slen check and the compressed
  slen/ulen check, matching Go's validation order).
- Go additionally needed `zstd.WithDecodeAllCapLimit` +
  `WithDecoderMaxMemory` on its shared decoder; this port already capped the
  decompression buffer at `ulen` (`zstd::bulk::decompress(stored, ulen)`),
  so the "bomb stops at ulen" behavior needed no code change — only the
  ulen bound itself was missing. The ported bomb test pins the existing
  behavior.

## Go PR #4 backport (2026-09-06)

- `Reader.Records()` → `Reader::records(self) -> Records<R>`, an iterator
  over `Result<RawRecord, Error>`; `RawRecord { record: Record, bytes:
  Vec<u8> }` stands in for Go's embedded-`Record` struct (`raw.record.key`
  for Go's promoted `raw.Key`). The decoding iterator (Go's `All`) is built
  on the same `next_record` step, exactly as Go rebuilt `All` on `Records`;
  both drive one fused state machine, so each yields at most one error and
  then `None`.
- `records` consumes the `Reader`: Go's "must be called at most once per
  Reader" is the type system's job here. The stream position is shared, so
  records already yielded through the decoding iterator are not re-read.
- Error classes and texts are the decoding iterator's (all `Malformed`, the
  corrupt-record text embedded), since the two share every check up to the
  decode. Tests ported from `pack_test.go`: the round trip (each record's
  bytes byte-equal to `encode_record`'s, the libzstd-compressed one
  included; re-added through `add_record` and decoded back), the truncated
  stream, the CRC mismatch; plus a fused-after-error pin mirroring the
  decoding iterator's.
