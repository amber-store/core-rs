# Filesystem Tree Storage

This document describes **how** the data types from [types.md](types.md) are
actually stored and combined into files and directories. It assumes the
[lookup key](keys.md) format and the type catalog in [types.md](types.md).

## Serialization: deterministic CBOR

Every structured object (`FileNode`, `DirLeaf`, `DirNode`, `XattrSet`) is encoded
with **deterministic CBOR per [RFC 8949 §4.2](https://www.rfc-editor.org/rfc/rfc8949#name-deterministically-encoded-c)**.
`Blob` objects are raw bytes with no framing.

Deterministic encoding is **required, not preferred**: a key's hash is computed over
the serialized bytes, so the same logical object *must* serialize to identical bytes
on every implementation and version, or content addressing and deduplication break.
The §4.2 rules used here:

- Definite-length encoding for all arrays, maps, strings, and byte strings
  (no indefinite-length items).
- Integers and lengths in the shortest form that fits.
- Map keys sorted by their encoded bytes, ascending.
- No floating-point values are emitted by any type defined here.

Hashing uses BLAKE3, truncated to fill the key as described in [keys.md](keys.md).

## Object layouts

### `Blob` (type 0)

Raw file-content bytes. No CBOR. A `Blob`'s key length field equals its byte length.

### `FileNode` (type 1)

A CBOR **array of child keys**, in file order:

```cbor
[ bstr<32> childKey, bstr<32> childKey, ... ]   ; child header = Blob or FileNode
```

No offsets are stored: each child key's length field already gives the number of
file bytes that child covers, so offsets are a running sum. The reference stream is
CDC-chunked at key boundaries, so wide files gain index levels automatically.

### `DirLeaf` (type 2)

A CBOR **array of entry maps**, sorted by `name` (bytewise lexicographic of the raw
name bytes). Entries are laid out sequentially with no intra-leaf index; a leaf is a
bounded CDC chunk, so a linear scan within it is cheap.

Each entry is a canonical CBOR **map with integer keys**. Absent optional keys cost
nothing (the key simply does not appear):

```cbor
{
  0: bstr  name,        ; single path component, raw bytes (no '/')
  1: uint  mode,        ; full POSIX st_mode — encodes BOTH type (S_IFMT) and perms
  2: uint  uid,
  3: uint  gid,
  4: int   mtime,       ; nanoseconds since the Unix epoch (may be negative)

  ; exactly one payload, selected by the type bits in `mode`:
  5: bstr<32> contentKey,           ; S_IFREG → Blob / FileNode
  ;                                   S_IFDIR → DirLeaf / DirNode, or a Commit (below)
  6: bstr     linkTarget,           ; S_IFLNK  → inline target path
  7: [uint major, uint minor],      ; S_IFCHR / S_IFBLK → device numbers
  ;                                   S_IFIFO / S_IFSOCK → none of 5/6/7

  ; optional, mutually exclusive:
  8: { bstr name => bstr value },   ; inline xattrs (small)
  9: bstr<32> xattrsKey,            ; spilled XattrSet key (large)
}
```

**A directory entry may hold a commit.** The content key of an `S_IFDIR` entry
is a `DirLeaf` or `DirNode` key, or the key of a [`Commit`](commits.md), which
stands for the directory that commit records (its tree; the first side, when the
tree is conflicted). Readers pass through it: `LookupEntry`, `ListEntries` and
`CollectEntries` take a commit key wherever they take a directory key and
continue with its tree, and `DirOf` names the directory a key stands for. A
commit's tree is never a commit, so one step suffices. The commit is a child of
the leaf like any content key, so its tree and its history are reachable,
required for completeness, and kept alive through the directory. A commit
stands nowhere else: not inside a directory's own index (the child of a
`DirNode` is a `DirNode` or a `DirLeaf`), and not under an entry of another
type. The codec does not hold a content key to its entry's mode, so the latter
is a malformed tree that decodes; path resolution refuses it.

Folding the file type into `mode` (the POSIX way) means there is no separate type
field; the reader masks `mode & S_IFMT` to learn which payload key is present.

### `DirNode` (type 3)

A CBOR **array of `[sepName, childKey]` pairs**, sorted by `sepName` — the highest
entry name contained in that child's subtree:

```cbor
[ [ bstr sepName, bstr<32> childKey ], ... ]   ; child header = DirLeaf or DirNode
```

The pair stream is itself CDC-chunked at pair boundaries, so very wide directories
gain index levels automatically.

### `XattrSet` (type 4)

A canonical CBOR **map** of extended attributes, used when a single entry's inline
xattrs (key 8 above) grow too large to keep in the leaf:

```cbor
{ bstr name => bstr value, ... }
```

Its key length field equals its own serialized byte length.

## Content-defined chunking

Two chunkers produce the trees; both are deterministic functions of content, so the
resulting trees are history-independent (identical content → identical tree →
maximal deduplication).

**Byte chunker (Blobs).** A rolling hash (e.g. gear/buzhash, FastCDC-style) over a
sliding window of file bytes sets `Blob` boundaries, parameterized by minimum,
average, and maximum chunk size.

**Item chunker (index and entry streams).** For `FileNode`, `DirLeaf`, and
`DirNode`, the boundary is decided **per item** (a child key, a directory entry, or
a `[sepName, childKey]` pair): hash the item's canonical encoding with BLAKE3 and
end the current chunk when the low `k` bits are zero (target average run = `2^k`).
Because the decision is evaluated only *between* whole items, an item is **never**
split across objects — the "smart chunking" requirement. Optional minimum/maximum
run lengths bound the variance. The rule is order-independent: a single changed item
shifts only nearby boundaries.

## Building a tree (bottom-up)

Files and directories are built the same way:

1. **Leaves.** Chunk the content into leaves — `Blob`s for a file, `DirLeaf`s for a
   directory's sorted entries.
2. **Promote.** If more than one leaf results, gather their keys (for files) or
   `[sepName, childKey]` pairs (for directories) into a reference stream, run the
   item chunker over it, and emit a level of `FileNode`/`DirNode` objects.
3. **Recurse** step 2 on each new level until a level produces a single object.
   That object is the **root**; its key is what a parent directory entry stores.

A single-chunk file therefore has a `Blob` root; a single-chunk directory has a
`DirLeaf` root.

### Computing the key length field

The writer fills each key's length field while building (see
[types.md](types.md#length-field-logical-size-not-serialized-size)):

- `Blob.length` = number of raw bytes.
- `XattrSet.length` = serialized byte length.
- `FileNode.length` = Σ `childKey.length` — the file's content size. It **excludes**
  this node's own bytes, preserving O(1) `stat` and O(log n) seek.
- `DirNode.length` = **own serialized bytes** + Σ `childKey.length`.
- `DirLeaf.length` = **own serialized bytes** + Σ over entries of `contentKey.length`
  (`S_IFREG`/`S_IFDIR`) + `xattrsKey.length` (entries with spilled xattrs). Other
  entry types add nothing beyond own bytes — their inline data is already counted in
  the serialized leaf. An entry that holds a `Commit` key adds that key's length
  like any other: the commit's own bytes plus the trees it records, never its
  parents ([commits.md](commits.md#the-key)).

## Read paths

**Open + seek a file.** Start at the file's root key. If it is a `Blob`, the bytes
are the content. If it is a `FileNode`, walk its child keys summing
`childKey.length` until the sum passes the target offset, descend into that child
with the offset adjusted by the preceding sum, and repeat. Cost: O(log n) fetches,
O(fanout) memory per level.

**Look up a path component.** Start at the directory's root key. In a `DirNode`,
binary-search `sepName`s for the first child with `sepName ≥ name` and descend; in a
`DirLeaf`, linear-scan entries for an exact `name` match. Cost: O(log n) fetches,
O(fanout) memory per level — never the whole directory.

**`readdir`.** Stream `DirLeaf`s left to right, yielding entries already in sorted
order, holding one root-to-leaf path at a time.

**`du`.** Read the directory's root key and report its length field directly — O(1).

## Worked example

Directory `/etc` containing a 3-chunk regular file `hosts` and a symlink
`rc → rc.d/rc`:

```
DirLeaf(/etc) = [
  { 0:"hosts", 1:0o100644, 2:0, 3:0, 4:<mtime>, 5:<FileNode key> },
  { 0:"rc",    1:0o120777, 2:0, 3:0, 4:<mtime>, 6:"rc.d/rc" },
]   ; entries sorted by name: "hosts" < "rc"

FileNode(hosts) = [ <Blob0 key>, <Blob1 key>, <Blob2 key> ]
Blob0, Blob1, Blob2 = raw bytes
```

Length fields: `Blob{0,1,2}.length` = their byte sizes;
`FileNode(hosts).length` = their sum = the size of `hosts` (excludes the
`FileNode`'s own bytes);
`DirLeaf(/etc).length` = own serialized bytes of the `DirLeaf` +
`FileNode(hosts).length` (the symlink's inline target is already counted in the
`DirLeaf`'s own bytes).
