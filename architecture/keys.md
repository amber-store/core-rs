# Lookup Keys

A lookup key identifies a piece of content in the Amber-Store. It encodes the **type** of the payload and a **hash** of it.

Every key is exactly **32 bytes** long, laid out as three contiguous fields:

| Field | Size | Purpose |
|-------|------|---------|
| Payload hash   | remaining bytes       | Truncated Blake3 hash of the content, byte-reversed |
| Payload length | 1–8 bytes             | Little-endian byte length of the content |
| Header byte    | 1 byte, the last      | Payload type and length-field size |

Because the total is fixed at 32 bytes, the three fields trade space against one another: the longer the payload-length field, the fewer bytes remain for the hash.

## Byte order

A key is built in two steps. The fields are first encoded header first — the header byte, the payload length big-endian, the truncated hash — and then all 32 bytes are reversed. The reversal is what puts the hash in front and the header byte last, makes the length read little-endian, and stores the hash in the reverse of digest order.

It is there for the sake of ordering. The header and the length take few distinct values: encoded header first, every small blob would begin with the same bytes, and keys in bytewise order would cluster by type and size. With the uniformly distributed hash leading, a sorted list of keys is spread evenly over the key space, and its first bytes are fit to bucket, shard or split on. The low-entropy fields sit together at the end.

## Header byte

The header byte is the key's last byte. It is split into three fields, from most- to least-significant bit:

- **4 bits — type:** describes the kind of payload (e.g. Tree, Blob etc.).
- **1 bit — reserved:** must always be `0`.
- **3 bits — length size:** the number of bytes used by the payload-length field. The stored value is offset by one: `0` means 1 byte, `1` means 2 bytes, …, `7` means 8 bytes. So the field is always 1–8 bytes long.

## Payload length

The total byte length of the content, in the 1–8 bytes just before the header byte, least significant byte first.

Several combinations of length value and length-field size are semantically equivalent, so a canonical encoding is enforced: the most significant byte of the payload-length field, the one next to the header byte, must never be `0` (no zero padding). The one exception is a length of zero, which is a single `0` byte.

There is a special case when the object type is a directory, the payload length will represent cumulative length of data in the whole subtree. A commit follows the same idea: its payload length is its own byte length plus the lengths of the trees it records, and its parent commits are not counted ([commits.md](commits.md#the-key)). [types.md](types.md#length-field-logical-size-not-serialized-size) states the rule for every type.

## Payload hash

The payload hash is the [Blake3](https://github.com/BLAKE3-team/BLAKE3) hash of the content, truncated to its leading bytes to fill the remaining space in the key:

```
hash length = 32 - 1 - <length-field size>
```

That is, 32 total bytes minus the 1 header byte minus however many bytes the payload-length field occupies. The truncated hash is stored reversed: the key's first byte is the last byte of the truncated hash, and the byte just before the payload length is the hash's first.

## Example

The key of an empty blob (type `0`, length `0`). The Blake3 hash of no bytes begins `af 13 49 b9 …`, and its first 30 bytes end in `… ca e4 1f`:

```
1fe4ca939accb712c1adc925cb9b49c9dc36ea4d40a0a6a1f9f5b94913af 00 00
└─ hash, 30 bytes, reversed ───────────────────────────────┘ │  └ header: type 0, 1 length byte
                                                             └ length 0
```
