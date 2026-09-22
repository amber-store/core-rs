# References

A **reference** is a global name pointing at a store key (a file, a
directory, or a [commit](commits.md)), recorded with its creator and creation time, with room for a
signature. References give roots names: `ingest --ref NAME` creates one, and
any `KEY[/PATH]` argument also accepts `ref:NAME[@PATH]`.

## The record

A reference is a canonical CBOR map (RFC 8949 §4.2 core-deterministic, the
same convention as fstree objects) with integer keys:

| CBOR key | Field | CBOR type | Notes |
| --- | --- | --- | --- |
| 0 | name | text string | global reference name |
| 1 | key | 32-byte byte string | pointed-to key, canonical per [keys.md](keys.md) |
| 2 | user | text string | creator identity; may be empty |
| 3 | created_at | int64 | ns since the Unix epoch |
| 4 | signature | byte string, omitted when absent | raw SSHSIG blob (see below) |
| 5 | public_key | byte string, omitted when absent | signer's public key, SSH wire format |

**Signing is a consumer concern.** The core stores keys 4 and 5 opaquely and
neither creates nor verifies signatures; the record format reserves them so
that signed records remain byte-compatible everywhere. The convention for
consumers that sign (the amber server and its clients): the **signature
payload** is the deterministic encoding of the record without key 4 — the
canonical bytes of `{0,1,2,3,5}` — so the signature covers the signer's
public key; key 4 holds an **SSHSIG v1** signature (the `ssh-keygen -Y sign`
format) over that payload, namespace `amber-store-ref`, SHA-512 message hash,
raw binary blob (not PEM-armored).

**Name rules:** 1–1024 bytes of valid UTF-8; no `@` (the ref/path separator)
and no control characters (< 0x20 or 0x7F). `/` is allowed
(`backups/2026/06`) but has no structural meaning — names are opaque strings,
compared whole.

**Field bounds:** the user string is limited to 1024 bytes (same character
rules as names, but `@` is allowed for email-style identities); a signature
may be at most 64 KiB. Decoders reject records whose bytes are not the
canonical deterministic encoding.

**Mutability:** references are overwritable; a put for an existing name
replaces the record unconditionally. There is no history.

## Storage

References live in a Pebble DB (the `refstore` package), conventionally at
`<store-dir>/refs/` next to the object store: DB key = name bytes, value =
the CBOR record verbatim. Write durability follows the store's sync flag.
Listing is an iterator scan in lexicographic name order.

## CLI

```sh
amber-store ingest --ref NAME DIR    # ingest and name the root
amber-store ref set NAME KEY         # name an existing key
amber-store ref list                 # name, key, date, user
amber-store ref get NAME             # print the key NAME points at
amber-store ref rm NAME              # delete the name; objects stay
amber-store ls ref:NAME[@PATH]       # any KEY[/PATH] argument accepts this
```
