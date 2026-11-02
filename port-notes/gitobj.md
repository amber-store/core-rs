# gitobj — port notes

Backport of Go PR #28 (`amber-store/core`, merge commit `42c2547`, tag `v0.11.0`): CAS type 6,
`GitSHA1`, a git object under its SHA-1. The format is specified in `architecture/git-objects.md`, copied
verbatim from the Go repository like the rest of `architecture/`.

| Go | Rust |
|----|------|
| `key.GitSHA1`, `key.GitSHA1Size` | `Type::GitSha1`, `key::GIT_SHA1_SIZE` |
| `key.NewGitSHA1`, `Key.GitSHA1Sum` | `Key::new_git_sha1`, `Key::git_sha1_sum -> Option<[u8; 20]>` |
| `key.ErrNonCanonicalGitKey` | `key::Error::NonCanonicalGitKey` |
| `key.ErrNotBlake3`, from `New` / `NewFromHash` | a panic in `Key::new` / `Key::new_from_hash`, which are infallible |
| `gitobj.Kind`, `Encode`, `Parse` | `gitobj::Kind`, `encode`, `parse` |
| `gitobj.KeyOf`, `Verify`, `Links`, `Link` | `gitobj::key_of`, `verify`, `links`, `Link` |
| `gitobj.ErrMalformed` / `ErrMismatch` / `ErrCollision` | `gitobj::Error::{Malformed, Mismatch, Collision}` |
| `fstree.Link`, `Links`, `LinkTo` | `fstree::Link`, `links`, `link_to` |
| `fstree.visits` | `fstree::read::Visits` (private) |
| `packstore.MarkSet.MarkWalked` | `MarkSet::mark_walked` |
| `versionGit`, `versionFor(k, flags)`, `versionForCodec` | `VERSION_GIT`, `version_for(k, flags)`, `version_for_codec` |

## Decisions a reviewer should check

- **Collision detection** is `sha1-checked` 0.10 here and `pjbgf/sha1cd` in Go, both
  ports of Marc Stevens's SHA-1DC. Either way a detected collision is refused, so
  the safe hash each computes for one is never used, and on anything else both give
  plain SHA-1. `gitobj.json` and `collision_is_refused` hold them to the same answer
  on the SHAttered prefix.
- **`new_from_hash` panics on `GitSha1`** rather than returning a `Result`: Go's
  `ErrNotBlake3` is a caller's bug, every other type stays infallible, and making
  the constructors fallible would touch every call site for a case nothing reaches.
  `verify_object` returns before computing a BLAKE3 digest, the one place that takes
  a type from a key.
- **`version_for` reads the type nibble from the raw byte**, not through
  `Key::type_`, which panics on a reserved one: it runs on what a scrub finds on disk.
  `parse_record` has refused a reserved type before then, so this is belt and
  braces.
- **The mode arithmetic shifts bits out** with `<<`, as Go's and C's unsigned
  arithmetic do, and does not use a checked operation: git reads such a mode, and
  `tree: mode shifts bits out` in `gitobj.json` holds the two to the same result.

- **`put_verified` reports a forged object as `Error::Verify`,** not as corruption,
  for every type. It used `Error::Corrupt { verify: true }`, whose `is_corrupt()` was
  true where Go's `errors.Is(err, ErrCorrupt)` is false. The text was and is the same.
- **`parallel_map` spawns scoped threads per BFS level,** which a long history makes
  visible: about five times Go's cost on a chain of 100,000 commits with an in-memory
  getter (2.0 s against 0.32 s, measured during other load). Correct, and left alone:
  a real store's reads weigh more. A threshold below which it runs sequentially is the
  obvious lever when this matters.

## How it was checked

- `tests/golden/gitobj.json`: every kind, good and bad, with Go's key, links and
  error text. `tests/golden/segments_go_git/`: a Go-written store of a git history
  at version 4, which Rust opens, walks, scrubs and resumes.
- `interop/check.sh`, section 7: the same history in both directions, written by one
  implementation, referenced and swept by the other, with the object nothing reaches
  gone afterwards and the rest intact.
- `src/gc/git_tests.rs` compares the walks with `git rev-list --objects` on a real
  repository, as Go's does.
- A throwaway differential oracle (deleted, per `PORTING.md`): 300,007 seeded mutated
  payloads through both implementations gave identical keys, links, error text and
  `verify` verdicts.
