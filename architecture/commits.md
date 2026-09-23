# Commits

A **commit** is a content-addressed snapshot record, the analogue of git's
commit object: it names a root directory and records the commits it follows,
who wrote and who recorded the change, when, and why. It is CAS object type 5
([types.md](types.md)), encoded like every structured object as deterministic
CBOR ([fstree.md](fstree.md#serialization-deterministic-cbor)).

Commits and [references](references.md) compose the way git's objects and
refs do. A reference whose key is a commit key is a **branch**: advancing it
is a reference put, and the history stays reachable through the parents. The
core defines the object and keeps it alive; branch, merge and log policy
belong to consumers.

## The record

A canonical CBOR map (RFC 8949 §4.2 core-deterministic) with integer keys,
the same convention as `DirLeaf` entries and reference records.

| CBOR key | Field | CBOR type | Notes |
| --- | --- | --- | --- |
| 0 | tree | 32-byte byte string | canonical key of type `DirLeaf` or `DirNode` |
| 1 | parents | array of 32-byte byte strings | canonical keys of type `Commit`; order is significant (the first parent is the mainline); the empty array for a root commit; no duplicates; at most 256 |
| 2 | author | identity map | who wrote the change |
| 3 | committer | identity map | who recorded the commit |
| 4 | message | text string | UTF-8, may be empty, at most 1 MiB |
| 5 | signature | byte string, omitted when absent | raw SSHSIG v1 blob, at most 64 KiB |
| 6 | public_key | byte string, omitted when absent | signer's public key, SSH wire format, at most 16 KiB |

Keys 0–4 are always present. An **identity** is a map whose four keys are
always present:

| CBOR key | Field | CBOR type | Notes |
| --- | --- | --- | --- |
| 0 | name | text string | 1–1024 bytes of valid UTF-8, no control characters (< 0x20 or 0x7F) |
| 1 | email | text string | 0–1024 bytes, same character rules |
| 2 | when | int64 | ns since the Unix epoch, the store's time convention |
| 3 | tz_offset | int | minutes east of UTC, −1439..1439 |

**Differences from git.** Time is in nanoseconds, not seconds. The message is
UTF-8 by definition, so there is no `encoding` header. There are no tag
objects, so no `mergetag`. There are no free-form extra headers: a map key
outside the tables above is a decoding error.

## Signing

**Signing is a consumer concern**, exactly as for references: the core stores
keys 5 and 6 opaquely and neither creates nor verifies signatures. The
convention for consumers that sign: the **signature payload** is the
deterministic encoding of the record without key 5 — the canonical bytes of
`{0,1,2,3,4,6}` — so the signature covers the signer's public key; key 5
holds an **SSHSIG v1** signature over that payload, namespace
`amber-store-commit`, SHA-512 message hash, raw binary blob (not
PEM-armored). The commit's key hashes the full bytes, signature included, as
in git: signing a commit changes its key.

## Decoding is strict

A decoder accepts only the bytes the encoder would produce for the same
record. Missing or unknown map keys, reordered keys, indefinite-length items,
non-minimal integers and lengths, trailing bytes, keys of the wrong object
type and values outside the bounds above are all rejected. One logical commit
therefore has exactly one encoding and one key.

## The key

Type 5. The key's length field is the commit's **own serialized byte
length** — the `Blob`/`XattrSet` rule, not the directories' subtree footprint
— so the store verifies it along with the hash. The size of the snapshot is
the length field of the tree key, one fetch away.

## Reachability

A commit's children are its **tree, then its parents in recorded order**
(`fstree.ChildKeys`). Every object-graph walk dispatches through that one
function, so:

- **Transfer** (`fstree.ReachableKeys`): sending a commit sends its whole
  history.
- **Completeness** (`fstree.CheckComplete`, and so `gc.PrepareRef`): a
  reference may name a commit only when its entire ancestry is present.
  There is no shallow history. A commit is an interior node, so a missing
  parent surfaces as the read error for that key.
- **Garbage collection**: history stays live while any reference reaches it.
  The mark prunes at already-marked keys, so trees shared between commits
  are walked once. Dropping the last reference makes the history garbage.

One cost to know: `PrepareRef` re-walks the closure on every reference put,
and with commits the closure is all of history, so a put grows with the
number of unique objects ever committed on that branch, not with the size of
the new commit.

## Golden vector

Pinned in `commit/commit_test.go`; other implementations must reproduce it
byte for byte.

- `tree`: the empty directory, a `DirLeaf` whose body is the empty CBOR array
  `80` with length field 1 —
  `2001bbe6a9f5a0146a1f4d0381e9b0ed1ac2f1a979ce9d5ad84e46ff0b58f36b`.
- Ann: name `Ann`, email `ann@example.com`, when `1700000000000000000`,
  tz_offset `120`. Bob: name `Bob`, empty email, when `1700000000000000001`,
  tz_offset `-300`.
- Parent A: a root commit of `tree`, author and committer Ann, message `a` —
  key `5073d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c`.
- Parent B: the same with message `b` —
  key `5073c8825d499d27183b57319a8b637c7868ef9783da1d19369231d0bbe48831`.
- The vector: tree `tree`, parents `[A, B]`, author Ann, committer Bob,
  message `merge\n`. 174 bytes, key
  `50ae7b19332c07e0f197c8a9d410c0cc3ed2d7fd6cf34d3a7cb3c43d6c514980`:

```
a5                                     ; map(5)
  00 5820 2001bbe6a9f5a0146a1f4d0381e9b0ed1ac2f1a979ce9d5ad84e46ff0b58f36b   ; 0: tree
  01 82                                ; 1: parents, array(2)
     5820 5073d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c
     5820 5073c8825d499d27183b57319a8b637c7868ef9783da1d19369231d0bbe48831
  02 a4                                ; 2: author, map(4)
     00 63 416e6e                      ;    0: "Ann"
     01 6f 616e6e406578616d706c652e636f6d   ; 1: "ann@example.com"
     02 1b 17979cfe362a0000            ;    2: 1700000000000000000
     03 18 78                          ;    3: 120
  03 a4                                ; 3: committer, map(4)
     00 63 426f62                      ;    0: "Bob"
     01 60                             ;    1: ""
     02 1b 17979cfe362a0001            ;    2: 1700000000000000001
     03 39 012b                        ;    3: -300
  04 66 6d657267650a                   ; 4: "merge\n"
```

## CLI

```sh
amber-store commit create --author 'Ann <ann@example.com>' -m MSG TREE   # print the new commit key
amber-store commit create --ref main --parent ref:main ... TREE          # advance a branch
amber-store commit show KEY | ref:NAME                                   # headers, then the message
amber-store ls ref:main@sub/dir                                          # a commit stands for its tree
```

`TREE` is any `KEY[/PATH]` or `ref:NAME[@PATH]` spec that resolves to a
directory. `--parent` takes a commit key or a reference to one, and repeats
for a merge, mainline first. `--committer` defaults to the author, and
`--date` (RFC 3339) to now in the local zone; the zone's offset is recorded.
`create` refuses a tree or parent that is not in the store. Wherever a
command takes a directory spec (`ls`, `export`, `restore`), a commit key or a
reference to a commit is accepted and resolves to the commit's tree.
