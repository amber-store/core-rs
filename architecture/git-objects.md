# Git Objects

The store can hold a git repository's objects as they are, under the ids git
gives them, so that a branch can be pushed to it and kept alive by a reference.
Type 6 of the [CAS object types](types.md), `GitSHA1`, is one kind of key for all
four kinds of git object.

Pushing itself is not here: this document covers what a store does with a git
object once it is given one.

## The key

```
ce013625030ba8dba906f756967f9e9ca394464a 00000000000000000000 00 60
└─ SHA-1, 20 bytes, in digest order ───┘ └─ 10 zero bytes ─┘  │  └ header: type 6, 1 length byte
                                                              └ length 0
```

[keys.md](keys.md#git-keys) has the rules. The two things that differ from every
other key are both consequences of what git's ids are:

- **The hash is not reversed.** The other types reverse theirs only for a side
  effect, and git's ids need none of it: a SHA-1 is as uniform at the front as a
  BLAKE3 digest is, which is all the packstore's index and filter ask of a key.
  In digest order, a key's hex starts with the id `git` prints, and keys sort as
  git ids do.
- **The length is always zero.** A git commit names its tree by id alone and a
  tree names its entries by id alone, so a key made from a link in another object
  could not know a size. The store cannot give a directory-style footprint for
  this reason as well, and does not try: a git key's length says nothing about
  the object.

Everything after the SHA-1 has to be zero, or the key does not validate. One git
object has one key.

## The payload

A record's payload is the exact bytes git takes the SHA-1 of: the frame and the
content.

```
<kind> <size>\0<content>
```

`kind` is `blob`, `tree`, `commit` or `tag`. `size` is decimal, with no leading
zeros, and equals the length of what follows. It is what a loose object in a git
repository holds once inflated. The record's own codec compresses it like any
other payload.

A git object is never chunked: its SHA-1 covers it whole, so it is one record, and
[`amberpack.MaxPayload`](amberpack.md) (256 MiB, the frame included) is the largest
git object a store can hold. Nor are there deltas: each version of a file is stored in full, and a
repository takes about what its loose objects would, more than a git packfile.

## Verification

Wherever the store verifies an object (`packstore`'s verifying writes, the
copies of a compaction, a scrub), a git object is held to more than a hash:

1. **No collision.** The SHA-1 is computed with collision detection, as git's own
   is: a payload that carries the marks of a known collision attack is refused
   (`gitobj.ErrCollision`). Plain SHA-1 can be forced to collide, and a store that
   dedups by key would keep whichever of two colliding objects came first, for
   everybody.
2. **The right SHA-1.** The payload hashes to the key.
3. **A frame.** The payload is framed as above: it is a git object at all.

The content is left alone. Reading it is the business of whoever has to, and that
comes down to the [walks](#links): an interior object that cannot be read is
refused by `CheckComplete` before a reference to it is accepted, so an accepted
reference stops a mark only if what it reaches was damaged since. An object only
ever named as a file is never parsed,
by git or by the store, and a repository may hold one that would not parse. If
verification parsed it, it could not be pushed, and written any other way, every
sweep that had to copy it would leave its pack behind.

`packstore.Put` verifies nothing, for this type as for any other. An unframed
payload written that way, under its SHA-1 and named as a file, passes
`CheckComplete`, and then keeps its pack and fails each cycle that has to copy it
([a bad live record](mark-sweep-gc.md#the-cycle)), as a wrong payload under a `Blob`
key does. Whoever takes objects from others uses a verifying write.

## Links

A git object's links are what git follows from it, and the walks of `fstree`
follow them too: `CheckComplete` before a reference is accepted, `ReachableKeys`
for a transfer, and the collector's mark. Whatever keeps a commit alive keeps its
history alive, as for the store's own commits.

| Object | Links to |
|--------|----------|
| `blob`   | nothing |
| `tree`   | every entry: directories, files and symlinks |
| `commit` | the `tree` line, then the `parent` lines straight after it |
| `tag`    | the `object` line |

- **A commit's links are its leading lines,** as git reads them. A later header
  or a message may say `parent`; a merge tag embeds `object` and `type` lines. A
  `parent ` too short to hold an id is not a parent line, and not an error. In the
  safe direction the walk asks less than git: it does not need an author.
- **A tag's type is a C string,** as git compares it: `type commit\0junk` says
  `commit`.
- **A mode decides what a tree entry is,** by its file-type bits, as git's
  `canon_mode` does: `040000` is a directory, `100000` and `120000` are blobs, and
  anything else is a gitlink.
- **A gitlink is no link.** It holds a submodule's commit, which lives in another
  repository; a repository is complete without it. A submodule has to be pushed
  apart from the repository that holds it.

Everything a walk reaches must be there: `CheckComplete` fails on a missing
object, so a reference to a head from a shallow or partial clone is refused.

So a push has to send what git reaches from the head, as `git rev-list --objects`
lists it and `git cat-file --batch` serves it. Enumerating what a repository stores
is not the same thing: git supplies the empty tree `4b825dc…` without storing it, so
`--batch-all-objects` can omit an object a commit names.

### Walking without reading blobs

A `GitSHA1` key cannot tell a blob from a commit, so a root has to be read to know.
From there on the referring object tells: a tree entry's mode, or a tag's `type`
line, says which targets are blobs. Walks test those for presence and do not read
them (`fstree.Link.Leaf`). So a collection costs what a repository's trees and
commits cost, not every version of every file.

That word is a claim, and two links can disagree: one entry may call an object a
file while another calls it a directory. Git goes by each entry's mode, and through
the directory entry it would read what lies below. So a walk must not prune on a
key it has only met through a leaf link: it records what it has *read* apart from
what it has *seen* (`fstree`'s `visits`, and `packstore.MarkSet.MarkWalked`, a
second bit per record that is allocated only when a store holds git objects).

## Older releases

Releases up to 0.10.0 do not know type 6. Their record parser calls a key of that
type corrupt, and their scan of an active segment would take the first one for a
torn tail and truncate there, losing it and every record after it, of any type.
Checked against the 0.10.0 binary with a segment's version byte lowered: an ingest
truncated the segment at the git record and lost a plain blob written after it.

So a git record is only ever written to a segment at format version 4, which those
releases refuse, the whole store, at open and without touching it
([packstore.md](packstore.md)). An older process that was already running when
the first such segment appeared has opened the store and is not refused then. It
goes on writing to its own segment; a lookup that misses or a sweep fails with the
unsupported-version error, and the version-4 files stay as they are. A wire pack
needs no such gate: their pack reader refuses a record with such a key as corrupt.

## What a git key is not

The store's own objects and git's do not mix.

- **File operations refuse a git key.** `ls`, `restore`, `export` and
  `fstree.DirOf` read the store's directories, and say that a git key is not one.
  Reading a pushed branch back as files is a separate piece of work.
- **A store's `Commit` cannot name a git key**, as tree, parent or conflict term.
- **A directory entry holding a git key** is not refused by the encoders, and the
  walks follow it, but no reader can read through it. Nothing in this repository
  writes one.
