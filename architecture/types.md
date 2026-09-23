# Data Types

This document describes **what** kinds of data the Amber-Store holds and how they
model a filesystem tree. The concrete on-the-wire encoding of each type — and how
objects combine into files and directories — lives in [fstree.md](fstree.md).

Every piece of content is identified by a 32-byte [lookup key](keys.md). The key's
header byte carries a **4-bit type** field; this document defines what those types
mean.

## Two type namespaces

Keep these separate — it is the foundation of the whole model:

1. **CAS object type** — the 4-bit field in the key header ([keys.md](keys.md)).
   It tells a reader what kind of object a key points to (raw bytes, a file index,
   a directory leaf, …). Readers dispatch on this.
2. **Filesystem entry type** — the POSIX `st_mode` file type
   (regular / directory / symlink / character / block / fifo / socket). This lives
   **inside a directory entry's metadata**, because symlinks and special files have
   **no CAS object at all**: their data is stored inline in the parent directory.

A regular-file or subdirectory entry holds a **content key** (and that key's header
reveals whether it is a `Blob`, `FileNode`, `DirLeaf`, or `DirNode` — or, for a
subdirectory, a `Commit`, which stands for the directory it records). A symlink,
device, fifo, or socket entry holds its small data **inline** and references no key.

## CAS object types

The 4-bit type field has 16 slots; 6 are defined.

| Type | Name       | Role                                                              | Key length field encodes        | Child object types   |
|------|------------|------------------------------------------------------------------|---------------------------------|----------------------|
| 0    | `Blob`     | Raw file-content byte chunk (a CDC leaf). A single-chunk file *is* one `Blob`. | own serialized byte length      | —                    |
| 1    | `FileNode` | File chunk-index node, keyed by byte offset (file content tree).  | **total file content bytes (= file size)** | `FileNode` / `Blob`  |
| 2    | `DirLeaf`  | A contiguous run of complete directory entries (prolly-tree leaf).| **own bytes + subtree footprint** | (entries are inline); their content keys: `Blob` / `FileNode`, `DirLeaf` / `DirNode` / `Commit`, `XattrSet` |
| 3    | `DirNode`  | Directory index node, keyed by entry name (directory tree).      | **own bytes + subtree footprint** | `DirNode` / `DirLeaf`|
| 4    | `XattrSet` | Spilled extended attributes, when too large to store inline.     | own serialized byte length      | —                    |
| 5    | `Commit`   | Snapshot record: a directory root (or the sides of a conflict), parent commits, author, committer, message, change id ([commits.md](commits.md)). | **own bytes + the footprints of its trees**; parents are not counted | `DirLeaf` / `DirNode` (tree and conflict terms), `Commit` (parents) |
| 6–15 | reserved   | Must not be emitted.                                             | —                               | —                    |

**Leaf and internal nodes are distinct types** (`Blob`/`FileNode`,
`DirLeaf`/`DirNode`) so the key alone tells a reader whether following it yields
data or more index — no fetch-then-discriminate. With 11 free slots this costs
nothing.

## Length field: logical size, not serialized size

The key's hash always covers the object's **serialized bytes**. The key's
**length field**, however, carries a *logical* size for the aggregate types — this
extends the rule [keys.md](keys.md) already states for directories:

- `Blob`, `XattrSet`: length = the object's own serialized byte length.
- `FileNode`: length = total bytes of the file region it covers — i.e. the file's
  **content size**, *excluding* this node's own index bytes. This buys **O(1)
  `stat`** (a file's size is read from the `contentKey` already inline in its parent
  `DirLeaf`, with no file fetch) and **O(log n) random-access seek** (each child
  key's length is exactly the content span it covers).
- `DirLeaf`, `DirNode`: length = **own serialized bytes + the cumulative length of
  every child** → a true `du`-style footprint of the directory subtree, in O(1) from
  any node.
- `Commit`: length = **own serialized bytes + the length of its tree and of every
  conflict term** → the footprint of the snapshot it records. Its **parents are
  not counted**, although they are children: a merge would count the history its
  parents share twice, doubling the value at every merge until the 8-byte field
  overflows, and a directory that holds a commit would report the size of a whole
  history ([commits.md](commits.md#the-key)). The commit's own bytes name its
  trees, so the store recomputes and verifies this length.

For directories the cumulative therefore counts, across the whole subtree: every
regular file's content bytes, every directory object's (`DirLeaf`/`DirNode`) own
serialized bytes, and every spilled `XattrSet`'s bytes. It does **not** count
`FileNode` index bytes — those belong to the file, whose key exposes only its
content size. A commit held by an entry counts with its own footprint: its bytes
and its trees, not its history. Concretely:

- `DirNode.length` = own serialized bytes + Σ `childKey.length`.
- `DirLeaf.length` = own serialized bytes + Σ over entries of `contentKey.length`
  (for `S_IFREG`/`S_IFDIR`) + `xattrsKey.length` (for entries with spilled xattrs).
  Symlinks, devices, fifos, and sockets add nothing beyond the `DirLeaf`'s own bytes,
  which already include their inline data.

## Filesystem entry types

A directory entry's metadata includes the full POSIX `st_mode`, whose high bits
(`S_IFMT`) give the entry type. The type determines where the entry's payload
lives:

| Entry type        | `S_IFMT`   | Payload                                   |
|-------------------|------------|-------------------------------------------|
| regular file      | `S_IFREG`  | content key → `Blob` or `FileNode`        |
| directory         | `S_IFDIR`  | content key → `DirLeaf` or `DirNode`, or a `Commit`, which reads as its tree |
| symbolic link     | `S_IFLNK`  | inline target path                        |
| character device  | `S_IFCHR`  | inline `rdev` (major, minor)              |
| block device      | `S_IFBLK`  | inline `rdev` (major, minor)              |
| fifo              | `S_IFIFO`  | none                                      |
| socket            | `S_IFSOCK` | none                                      |

Each entry also carries `uid`, `gid`, `mtime`, and optional extended attributes.
File **size** is *not* stored in the entry — it is read from the content key's
length field, avoiding duplication.

## The directory model

A directory is a **prolly tree**: a map from entry name → entry, sorted by name and
split into CAS objects by content-defined chunking applied **at entry boundaries**
(an entry is never split across objects). `DirLeaf` objects hold runs of entries;
`DirNode` objects index child subtrees by name.

This gives the project's primary goal — large directories (>100K entries) processed
with **less than O(n) memory**:

- **Lookup** of one name: O(log n) object fetches, each O(fanout) memory.
- **Ordered iteration** (`readdir`): stream leaves left-to-right, holding one
  root-to-leaf path at a time.
- **Mutation** of one entry: re-chunk the affected leaf and re-index the O(log n)
  objects on its path; the rest is structurally shared with — and deduplicated
  against — the prior tree.

A directory whose entries fit in a single chunk has a `DirLeaf` root; a larger one
has a `DirNode` root. The parent entry stores only the root key; its header type
discriminates.

## The file model

A regular file's content is split into chunks by content-defined chunking. Each
chunk is a `Blob`. A single-chunk file *is* one `Blob`; a larger file gets a
`FileNode` index whose child-reference stream is itself CDC-chunked, producing
multiple levels so even multi-GB files keep an O(log n) index and O(log n)
random-access seek. Every file — even one byte — is its own CAS object (no
inlining of file content into the directory entry), keeping the model uniform.

## Root directory metadata

The top of a store is simply a directory key (`DirLeaf` or `DirNode`). Because
entry metadata lives in the *parent* entry, the **root directory carries no
metadata of its own** for now — whatever external reference names the root key can
hold root ownership/mode/mtime later if needed.

## Commits

A [`Commit`](commits.md) is the snapshot record in content-addressed form: it
names a root directory and carries who recorded it, when, why, and which
commits it follows; for [jj](https://jj-vcs.github.io/jj/) it also carries a
change id and, when the tree is conflicted, every side of the conflict. Its
children in the object graph are its tree, the further conflict terms and its
parent commits, so whatever keeps a commit alive keeps its history alive.

A directory entry may hold a commit where a subdirectory's key would stand
([commits.md](commits.md#commits-inside-directories)). Every file operation
reads through it to the commit's tree, and the commit object itself is skipped;
the object graph keeps the commit and its history alive with the directory.
