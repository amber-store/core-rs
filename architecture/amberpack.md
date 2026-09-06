# Amberpack

The **amberpack** format is how Amber-Store frames content-addressed objects as a
byte stream. One record codec serves two consumers: packstore's on-disk segment
files and the wire packs that move objects between stores. The shared unit is
the **record** — a self-describing, CRC-protected, individually-zstd-compressed
CAS object. This document specifies the record and the wire pack that frames a
set of them. The on-disk segment format (which wraps the same records in a
header and a self-indexing footer) lives in
[packstore's package doc](../packstore/segment.go).

All integers are big-endian, the project-wide convention.

## The record

A record is the atomic unit. Its header is a fixed **46 bytes**, followed by the
stored payload:

```
offset  size  field
0       1     tag      0x01 (tagChunk)
1       32    key      the object's 32-byte lookup key
33      1     flags    bit 0 (0x01) = zstd; all other bits reserved (must be 0)
34      4     ulen     uncompressed payload length
38      4     slen     stored payload length (on the wire / on disk)
42      4     crc      CRC-32C (Castagnoli) over the whole record
46      slen  payload  raw object bytes, or zstd frame when the zstd flag is set
```

Each object is identified by its 32-byte [key](keys.md), whose header byte
encodes the [CAS object type](types.md) and a length field. The key here is
written verbatim; **canonical-form validation happens on the read side**, never
on write.

**Compression is opportunistic and per-record.** `EncodeRecord` zstd-compresses
the payload only when the result is *strictly* smaller than the original; if it
isn't, the payload is stored raw and the zstd flag stays clear. Two invariants
follow and are enforced on parse:

- raw record (`flags & zstd == 0`) ⟹ `ulen == slen`
- compressed record (`flags & zstd != 0`) ⟹ `slen < ulen`

The `ulen`/`slen` split lets a reader size its decompression buffer exactly and
detect a payload that decompresses to the wrong length.

**The CRC covers the whole record with its own field zeroed.** It is computed
over bytes `[0:42]`, then four zero bytes standing in for the `crc` field, then
the payload `[46:46+slen]`. A reader recomputes the same way and rejects a
mismatch. The CRC guards framing and bytes; it is *not* a substitute for the
payload hash check (below).

Both length fields are `uint32`, so a single object's payload is capped at 4 GiB
— far above the chunker's output, which is on the order of a few hundred KiB.

### Two checks, two layers

A record carries integrity at two independent layers, and they answer different
questions:

1. **CRC-32C** — "did these bytes survive storage/transit intact?" Checked
   whenever a record is parsed.
2. **Payload hash** — "is this really the object its key claims?" The key embeds
   a BLAKE3 digest of the object's serialized bytes ([keys.md](keys.md)). The
   record codec does **not** verify this; it is re-checked in the storage path
   (packstore's parallel write with verification) before anything is committed.

Keeping the hash check out of the codec is deliberate: the codec is a framing
layer, and the storage layer is the single authoritative gate on object
identity. A pack stream is therefore trusted only for framing, never for
content. A record read from a pack can also be appended to a store as it is
(packstore's `Object.Record`): the store re-parses it, and with verification
decodes and rehashes the payload, so the gate is the same whichever form an
object arrives in.

## The wire pack

A wire pack is a possibly-partial, unordered set of CAS objects carrying **no
root key** — like a git pack. It is the unit a store's `inbox` receives, and
the unit object transfer between stores is built on. Layout:

```
Magic    "AMBERPK\x03"   8 bytes, plaintext
Records  zero or more — each one EncodeRecord output (46-byte header + payload)
End      0x00            one byte (tagEnd)
```

The first byte of a record is `0x01` (`tagChunk`) and the end marker is `0x00`
(`tagEnd`), so the two are unambiguous on that first byte. The explicit end
marker is the point of the framing: a stream that is cut short stops before
`0x00`, so **truncation is detected as a malformed stream rather than read as a
clean EOF**. A pack with no records is still valid — magic immediately followed
by the end marker.

### Reading

The reader streams records one at a time. For each it reads the 46-byte header,
bounds-checks `slen` against a 256 MiB per-record limit (a guard against a
hostile or corrupt stream triggering an unbounded allocation), reads the
payload, then validates the assembled record with the record parser: framing,
flags, the raw/compressed length invariants, the CRC, and **key canonicality**.
A bad tag, a truncated header or payload, an oversized record, or a failed
record check all surface as a single malformed-stream error that stops
iteration. The reader decodes each payload but, as noted above, does not verify
the payload hash.

### Magic and versioning

The trailing byte of the magic is the format version. Only `\x03` is produced
and accepted today. Versions `\x01` (uncompressed whole-stream) and `\x02`
(whole-stream zstd) were earlier stream formats; they are no longer written and
are **rejected** by the reader. Bumping the version byte is the migration lever
if the framing ever changes incompatibly.

### Error classes

The codec exposes two `errors.Is` sentinels, so consumers can map framing and
content failures to distinct error classes:

- **malformed pack stream** — stream-level: bad or legacy magic, truncation
  before the end marker, an oversized record, or a record that fails its own
  check.
- **corrupt pack data** — record-level: bad framing, unknown flags, a CRC
  mismatch, a length inconsistency, or a non-canonical key. packstore reuses
  this same sentinel for its footer- and scrub-level corruption, so a single
  target covers record-, footer-, and segment-level damage.

## Why one codec for disk and wire

packstore writes these exact records into its append-only segment files; a wire
pack is just those records bracketed by a magic and an end marker. Sharing the
codec means an object compressed once on disk can be copied byte-for-byte into a
wire pack without re-encoding, and a received record can be appended to a segment
without re-framing. The CRC and the raw/compressed invariants are checked
identically in both settings, and the BLAKE3 payload hash remains the single
authoritative identity gate wherever an object is about to be stored.
