# Lookup Keys

A lookup key identifies a piece of content in the Amber-Store. It encodes the **type** of the payload and a **hash** of it.

Every key is exactly **32 bytes** long, laid out as three contiguous fields:

| Field | Size | Purpose |
|-------|------|---------|
| Header byte    | 1 byte                | Payload type and length-field size |
| Payload length | 1–8 bytes             | Big-endian byte length of the content |
| Payload hash   | remaining bytes       | Truncated Blake3 hash of the content |

Because the total is fixed at 32 bytes, the three fields trade space against one another: the longer the payload-length field, the fewer bytes remain for the hash.

## Header byte

The header byte is split into three fields, from most- to least-significant bit:

- **4 bits — type:** describes the kind of payload (e.g. Tree, Blob etc.).
- **1 bit — reserved:** must always be `0`.
- **3 bits — length size:** the number of bytes used by the payload-length field. The stored value is offset by one: `0` means 1 byte, `1` means 2 bytes, …, `7` means 8 bytes. So the field is always 1–8 bytes long.

## Payload length

A Big-Endian encoding of the total byte length of the content.

Several combinations of length value and length-field size are semantically equivalent, so a canonical encoding is enforced: the first byte of the payload-length field must never be `0` (no leading-zero padding).

There is a special case when the object type is a directory, the payload length will represent cumulative length of data in the whole subtree. A commit follows the same idea: its payload length is its own byte length plus the lengths of the trees it records, and its parent commits are not counted ([commits.md](commits.md#the-key)). [types.md](types.md#length-field-logical-size-not-serialized-size) states the rule for every type.

## Payload hash

The payload hash is the [Blake3](https://github.com/BLAKE3-team/BLAKE3) hash of the content, truncated to fill the remaining space in the key:

```
hash length = 32 - 1 - <length-field size>
```

That is, 32 total bytes minus the 1 header byte minus however many bytes the payload-length field occupies.
