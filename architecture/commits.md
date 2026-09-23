# Commits

A **commit** is a content-addressed snapshot record, the analogue of git's
commit object: it names a root directory and records the commits it follows,
who wrote and who recorded the change, when, and why. It also carries what a
[jj](https://jj-vcs.github.io/jj/) commit has besides: a change id, and the
further sides of a conflicted tree. It is CAS object type 5
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
| 0 | tree | 32-byte byte string | canonical key of type `DirLeaf` or `DirNode`; in a conflicted commit, the first term |
| 1 | parents | array of 32-byte byte strings | canonical keys of type `Commit`; order is significant (the first parent is the mainline); the empty array for a root commit; no duplicates; at most 256 |
| 2 | author | identity map | who wrote the change |
| 3 | committer | identity map | who recorded the commit |
| 4 | message | text string | UTF-8, may be empty, at most 1 MiB |
| 5 | signature | byte string, omitted when absent | raw SSHSIG v1 blob, at most 64 KiB |
| 6 | public_key | byte string, omitted when absent | signer's public key, SSH wire format, at most 16 KiB |
| 7 | change_id | byte string, omitted when absent | 1–64 bytes, opaque: an identity that follows the change when the commit is rewritten |
| 8 | conflict_terms | array of 32-byte byte strings, omitted when absent | the terms of a conflicted tree after the first ([Conflicts](#conflicts)): an even number, 2–254, each a canonical `DirLeaf` or `DirNode` key; a key may repeat |
| 9 | conflict_labels | array of text strings, omitted when absent | only with key 8: one label per term counting the tree, so `1 + len(conflict_terms)` of them; each 0–65536 bytes of valid UTF-8 with no code point below U+0020 and no U+007F; at least one non-empty |

Keys 0–4 are always present; 5–9 are omitted when absent, so a commit that
uses none of them is five entries long. An **identity** is a map whose four
keys are always present:

| CBOR key | Field | CBOR type | Notes |
| --- | --- | --- | --- |
| 0 | name | text string | 0–1024 bytes of valid UTF-8 with no code point below U+0020 and no U+007F (nothing else counts as a control character here: U+0085 or U+2028 pass); empty for a user who configured none |
| 1 | email | text string | 0–1024 bytes, same character rules |
| 2 | when | int64 | ns since the Unix epoch, the store's time convention |
| 3 | tz_offset | int | minutes east of UTC, −1439..1439 |

**Differences from git.** Time is in nanoseconds, not seconds. The message is
UTF-8 by definition, so there is no `encoding` header. There are no tag
objects, so no `mergetag`. There are no free-form extra headers: a map key
outside the tables above is a decoding error. A tree may be conflicted, and a
commit may carry a change id.

## Conflicts

A **conflicted commit** records not one tree but the sum
`A0 − R0 + A1 − R1 + …` of trees added and removed, as jj does. Key 0 holds
`A0`; key 8 holds the rest in that order, *remove, add, remove, add, …*, so
their number is even. Key 9, when present, labels every term, the tree
included, and an empty string leaves a term unlabelled; a conflict without
any label carries no key 9 at all.

Wherever a commit stands for a directory (`ls`, `export`, `restore`, a
directory entry, below) it stands for key 0, the first side. All sides are
children in the object graph, so they travel and stay alive with the commit.

| jj `backend::Commit` | here |
| --- | --- |
| `root_tree: Merge<TreeId>`, values `[A0, R0, A1, …]` | key 0 is `A0`, key 8 the rest |
| `conflict_labels: Merge<String>` | key 9 |
| `change_id` | key 7 |
| `parents`, `description`, `author`, `committer` | keys 1–4; milliseconds map into nanoseconds losslessly within about 292 years of 1970, and an adapter refuses what lies outside |
| `secure_sig` | key 5; jj's signing callback signs bytes the backend chooses, and the payload below is such bytes |
| `predecessors` | not stored: deprecated in jj, which keeps them in its operation log; they would also be the only non-owning edge in the object graph |
| the virtual root commit | not an object: a commit whose only jj parent is the root has no parents here |

**Labels as jj makes them.** jj builds a label from a commit's short change
and commit ids and the whole first line of its description, with every
character that Rust's `char::is_control` names removed, and may add a note
such as `(rebased revision)`. Such text passes the character rule above,
which forbids less than jj removes. The first line of a description has no
bound, hence the generous 64 KiB; an adapter truncates what is longer.

## Signing

**Signing is a consumer concern**, exactly as for references: the core stores
keys 5 and 6 opaquely and neither creates nor verifies signatures. The
convention for consumers that sign: the **signature payload** is the
deterministic encoding of the record without key 5 — every other key that is
present, the new ones included — so the signature covers the signer's public
key; key 5 holds an **SSHSIG v1** signature over that payload, namespace
`amber-store-commit`, SHA-512 message hash, raw binary blob (not
PEM-armored). That form is a convention of amber's own consumers, not a rule
of the format: a jj backend stores whatever its signer returns, armored SSH
or GPG text included. The commit's key hashes the full bytes, signature
included, as in git: signing a commit changes its key.

## Decoding is strict

A decoder accepts only the bytes the encoder would produce for the same
record. Missing or unknown map keys, reordered keys, indefinite-length items,
non-minimal integers and lengths, trailing bytes, keys of the wrong object
type and values outside the bounds above are all rejected; so are an empty
array under key 8 or 9, an empty byte string under key 7, labels without
terms, and labels that are all empty, since absence has one encoding. One
logical commit therefore has exactly one encoding and one key.

## The key

Type 5. The key's length field is a **footprint**, as a directory's is: the
commit's **own serialized byte length plus the length fields of its tree and
of every conflict term**. It is the size of the snapshot the commit records,
plus the commit's own few hundred bytes, and the commit's bytes suffice to
recompute it, so the store verifies it along with the hash. That means
decoding: bytes under a `Commit` key that do not decode strictly fail
verification, whatever their length. A sum that does not fit 64 bits is an
encoding error, and such a commit has no key.

**Parent commits are not counted**, although they are children in the object
graph:

- A parent's length would hold its own tree and parents, so a merge would
  count the history its two parents share twice. A branch is cut from a recent
  commit, so both parents of its merge carry nearly the whole history, and
  every merge roughly doubles the value. The field is at most 8 bytes: a
  repository with a 1 GiB history would overflow it after about 34 merges,
  and the commit could not be keyed at all.
- A directory may hold a commit ([below](#commits-inside-directories)), and a
  directory's length is the `du`-style size of what is beneath it. With
  parents counted, such a directory would report the size of a whole history.

**Changed after v0.0.9.** That release keyed a commit with its own byte length
alone, and such a key no longer matches its object. What that means for a
store that holds one:

- The graph walks and the directory readers refuse it, with an error that
  names the footprint rule. So no reference can be put on it or on history
  built on it, `commit create` refuses it as a parent, and `ls`, `export` and
  `restore` do not read through it. `commit show` still prints it, which is
  how its tree is found again.
- The store's verification refuses it too. **While a reference keeps it
  alive, every gc pass that has to copy it, and every scrub, fails and reports
  corrupt pack data**; a release from before this change collects the same
  store without complaint.
- There is no migration. Delete the references that reach such commits
  (`ref rm`), after which gc collects them as garbage, which is not verified,
  and create the commits again from their trees.

A commit that uses keys 7–9 cannot be decoded by v0.0.9, which rejects unknown
keys.

## Reachability

A commit's children are its **tree, then the further conflict terms, then its
parents, all in recorded order** (`fstree.ChildKeys`). Every object-graph walk
dispatches through that one function, so:

- **Transfer** (`fstree.ReachableKeys`): sending a commit sends its whole
  history, and every side of a conflict.
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

## Commits inside directories

A directory entry of type `S_IFDIR` may carry a **`Commit` key as its content
key**, where otherwise a `DirLeaf` or `DirNode` key stands
([fstree.md](fstree.md#dirleaf-type-2)). It reads as the directory the commit
records:

- `fstree.LookupEntry`, `ListEntries` and `CollectEntries` take a commit key
  wherever they take a directory key and continue with its tree, and so does
  everything built on them: path resolution, `tarexport.Write`, and the CLI's
  `ls`, `export` and `restore`. `fstree.DirOf` names the directory a key
  stands for. A commit's tree is never a commit, so one step suffices.
- **File operations skip the commit object**: a tar of such a tree holds a
  plain directory, and a restore creates one.
- The object graph needs nothing special. A `DirLeaf`'s children are its
  entries' content keys whatever their type, so a directory that holds a
  commit keeps the commit, its tree **and its history** alive, requires all of
  it for completeness, and transfers it.
- The directory's own length adds the entry's content-key length, as for any
  entry; by the rule above that is the snapshot's footprint.
- Nowhere else. Inside a directory's own index the child of a `DirNode` is a
  `DirNode` or a `DirLeaf`, and the readers refuse a commit there. The codec
  does not hold an entry's content key to its mode, for commits no more than
  for files and directories: a commit under an entry that is not `S_IFDIR` is
  a malformed tree, which path resolution refuses and file operations fail on.

This is where a vendored tree with its history, or jj's
`TreeValue::GitSubmodule`, has a place. `ingest` never creates such an entry:
it reads a filesystem, which has none.

## Golden vectors

Pinned in `commit/commit_test.go`; other implementations must reproduce them
byte for byte.

- `tree`: the empty directory, a `DirLeaf` whose body is the empty CBOR array
  `80` with length field 1 —
  `2001bbe6a9f5a0146a1f4d0381e9b0ed1ac2f1a979ce9d5ad84e46ff0b58f36b`.
- Ann: name `Ann`, email `ann@example.com`, when `1700000000000000000`,
  tz_offset `120`. Bob: name `Bob`, empty email, when `1700000000000000001`,
  tz_offset `-300`.
- Parent A: a root commit of `tree`, author and committer Ann, message `a`:
  115 bytes, length field 115 + 1 —
  key `5074d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c`.
- Parent B: the same with message `b` —
  key `5074c8825d499d27183b57319a8b637c7868ef9783da1d19369231d0bbe48831`.

**The merge**: tree `tree`, parents `[A, B]`, author Ann, committer Bob,
message `merge\n`. 174 bytes, length field 174 + 1 = 175 (`af`), key
`50afd48693e2ae8418fb83d0459174df45ba5fdddef67376e74ac6893a5abbed`:

```
a5                                     ; map(5)
  00 5820 2001bbe6a9f5a0146a1f4d0381e9b0ed1ac2f1a979ce9d5ad84e46ff0b58f36b   ; 0: tree
  01 82                                ; 1: parents, array(2)
     5820 5074d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c
     5820 5074c8825d499d27183b57319a8b637c7868ef9783da1d19369231d0bbe48831
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

**The conflict**: tree `tree`; conflict terms a `DirLeaf` key of length 300
whose hash bytes are all `11` and a `DirNode` key of length 70000 whose hash
bytes are all `22` (the codec fetches no tree, so fabricated keys serve);
labels `ours`, the empty string, `theirs`; change id `00 01 … 0f`; parent A;
author Ann; a committer **without a name**, email `bot@example.com`, at Bob's
time and zone; message `conflict\n`. 258 bytes, length field
258 + 1 + 300 + 70000 = 70559 (`01139f`), key
`5201139f190aa234ad713392987671d47e7e3292a613c62e3d14a85d614b300b`:

```
a8                                     ; map(8): keys 0-4, 7, 8, 9
  00 5820 2001bbe6a9f5a0146a1f4d0381e9b0ed1ac2f1a979ce9d5ad84e46ff0b58f36b   ; 0: tree
  01 81                                ; 1: parents, array(1)
     5820 5074d980bd63330e7b37ddd0989bea896cd6a35988e973dfc4b1b28808930a7c
  02 a4                                ; 2: author, as above
     00 63 416e6e
     01 6f 616e6e406578616d706c652e636f6d
     02 1b 17979cfe362a0000
     03 18 78
  03 a4                                ; 3: committer
     00 60                             ;    0: "", no name
     01 6f 626f74406578616d706c652e636f6d   ; 1: "bot@example.com"
     02 1b 17979cfe362a0001
     03 39 012b
  04 69 636f6e666c6963740a             ; 4: "conflict\n"
  07 50 000102030405060708090a0b0c0d0e0f    ; 7: change_id, bytes(16)
  08 82                                ; 8: conflict_terms, array(2): remove, add
     5820 21012c1111111111111111111111111111111111111111111111111111111111
     5820 3201117022222222222222222222222222222222222222222222222222222222
  09 83                                ; 9: conflict_labels, array(3)
     64 6f757273                       ;    "ours"
     60                                ;    ""
     66 746865697273                   ;    "theirs"
```

## CLI

```sh
amber-store commit create --author 'Ann <ann@example.com>' -m MSG TREE   # print the new commit key
amber-store commit create --ref main --parent ref:main ... TREE          # advance a branch
amber-store commit create --change-id HEX ... TREE                       # carry a change id
amber-store commit show KEY | ref:NAME                                   # headers, then the message
amber-store ls ref:main@sub/dir                                          # a commit stands for its tree
```

`TREE` is any `KEY[/PATH]` or `ref:NAME[@PATH]` spec that resolves to a
directory, or to a commit, whose tree is then recorded. `--parent` takes a
commit key or a reference to one, and repeats for a merge, mainline first.
`--committer` defaults to the author, and `--date` (RFC 3339) to now in the
local zone; the zone's offset is recorded. `create` refuses a tree or parent
that is not in the store, a parent whose key does not carry its footprint, and
an empty `--change-id`. `show` prints a conflicted tree's further terms as
`conflict-remove KEY` and `conflict-add KEY` lines after `tree`, the labels
that are not empty as `conflict-label N TEXT` (N counts from 0, the tree),
and `change-id HEX` after the parents. Wherever a command takes a directory
spec (`ls`, `export`, `restore`), a commit key, a reference to a commit, or a
path through a directory entry that holds a commit is accepted and resolves
to the commit's tree.
