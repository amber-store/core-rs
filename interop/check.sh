#!/usr/bin/env bash
# Live Go <-> Rust interoperability check.
#
# Requires a checkout of the Go implementation (github.com/amber-store/core)
# at $AMBER_GO_REPO (default: ../amber-store-core) with the Go toolchain available,
# and cargo for this repository.
#
# Verifies, on a freshly created tree:
#   1. identical ingest root keys from both implementations;
#   2. each implementation reads the store directory the OTHER one wrote
#      (ls output byte-identical, export tars byte-identical);
#   3. a Rust restore of the Go-written store re-ingests (with Go) to the
#      same root key.
#   4. identical commit keys from both implementations for identical
#      inputs, and each implementation shows and lists the commits the OTHER
#      one wrote.
#   5. references live in one shared SQLite file: each implementation reads
#      (ref get, ref list, ls ref:NAME@PATH) the references the OTHER one
#      wrote into the same store directory, moves them with --expect, is
#      refused with a stale expectation, and both see the final state.
set -euo pipefail

RS_REPO="$(cd "$(dirname "$0")/.." && pwd)"
GO_REPO="${AMBER_GO_REPO:-$RS_REPO/../amber-store-core}"
[ -d "$GO_REPO/cmd/amber-store" ] || {
  echo "Go implementation not found at $GO_REPO (set AMBER_GO_REPO)" >&2
  exit 1
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "== building CLIs"
(cd "$RS_REPO" && cargo build -q --example amber-store)
RS="$RS_REPO/target/debug/examples/amber-store"
(cd "$GO_REPO" && go build -o "$WORK/amber-store-go" ./cmd/amber-store)
GO="$WORK/amber-store-go"

echo "== creating test tree"
T="$WORK/tree"
mkdir -p "$T/docs/deep/deeper" "$T/logs"
printf 'hello amber\n' > "$T/hello.txt"
head -c 3000000 /dev/urandom > "$T/big.bin"        # multi-chunk, multi-level index
touch "$T/empty"
printf 'x' > "$T/one-byte"
ln -s ../hello.txt "$T/docs/link"
printf 'nested content\n' > "$T/docs/deep/deeper/leaf.txt"
printf 'keep\n' > "$T/logs/keep.log"
printf 'drop\n' > "$T/logs/drop.tmp"
printf '*.tmp\n' > "$T/.amberignore"
mkfifo "$T/fifo"
touch -t 200001020304.05 "$T/docs/deep/deeper/leaf.txt"
if command -v xattr >/dev/null 2>&1; then
  xattr -w user.check interop "$T/hello.txt" 2>/dev/null || true
fi

echo "== ingest with both implementations"
ROOT_GO=$("$GO" --store "$WORK/store-go" ingest "$T")
ROOT_RS=$("$RS" --store "$WORK/store-rs" ingest "$T")
echo "   go:   $ROOT_GO"
echo "   rust: $ROOT_RS"
[ "$ROOT_GO" = "$ROOT_RS" ] || { echo "FAIL: root keys differ" >&2; exit 1; }

echo "== cross-reading each other's stores (ls)"
"$GO" --store "$WORK/store-go" ls --keys "$ROOT_GO" > "$WORK/ls-go-own.txt"
"$RS" --store "$WORK/store-go" ls --keys "$ROOT_GO" > "$WORK/ls-rs-cross.txt"
cmp "$WORK/ls-go-own.txt" "$WORK/ls-rs-cross.txt"
"$RS" --store "$WORK/store-rs" ls --keys "$ROOT_RS" > "$WORK/ls-rs-own.txt"
"$GO" --store "$WORK/store-rs" ls --keys "$ROOT_RS" > "$WORK/ls-go-cross.txt"
cmp "$WORK/ls-rs-own.txt" "$WORK/ls-go-cross.txt"

echo "== cross-exporting (tar byte-compare, 4 combinations)"
"$GO" --store "$WORK/store-go" export -o "$WORK/go-from-go.tar" "$ROOT_GO"
"$RS" --store "$WORK/store-go" export -o "$WORK/rs-from-go.tar" "$ROOT_GO"
"$RS" --store "$WORK/store-rs" export -o "$WORK/rs-from-rs.tar" "$ROOT_RS"
"$GO" --store "$WORK/store-rs" export -o "$WORK/go-from-rs.tar" "$ROOT_RS"
cmp "$WORK/go-from-go.tar" "$WORK/rs-from-go.tar"
cmp "$WORK/go-from-go.tar" "$WORK/rs-from-rs.tar"
cmp "$WORK/go-from-go.tar" "$WORK/go-from-rs.tar"

echo "== restore (rust, from the go store) -> re-ingest (go) -> same root"
"$RS" --store "$WORK/store-go" restore "$ROOT_GO" "$WORK/restored"
ROOT_AGAIN=$("$GO" --store "$WORK/store-check" ingest "$WORK/restored")
[ "$ROOT_GO" = "$ROOT_AGAIN" ] || { echo "FAIL: restored tree re-ingests to $ROOT_AGAIN" >&2; exit 1; }

echo "== refs (one shared refs.sqlite: each side reads and moves what the OTHER wrote)"
fail() { echo "FAIL: $*" >&2; exit 1; }
# A second root in each store, so a reference has somewhere to move to.
mkdir -p "$WORK/tree2"
printf 'second tree\n' > "$WORK/tree2/two.txt"
ROOT2_GO=$("$GO" --store "$WORK/store-go" ingest "$WORK/tree2")
ROOT2_RS=$("$RS" --store "$WORK/store-rs" ingest "$WORK/tree2")
[ "$ROOT2_GO" = "$ROOT2_RS" ] || fail "second root keys differ"
ROOT2=$ROOT2_GO
# In each round WRITER created the store and writes the reference; READER is
# the other implementation, working in the same store directory.
for writer in go rust; do
  if [ "$writer" = go ]; then
    WRITER="$GO"; READER="$RS"; STORE="$WORK/store-go"
  else
    WRITER="$RS"; READER="$GO"; STORE="$WORK/store-rs"
  fi
  "$WRITER" --store "$STORE" ref set nightly "$ROOT_GO"
  "$WRITER" --store "$STORE" ref set --expect none other "$ROOT2"
  # The other side reads them: get, list, and a path through the reference.
  [ "$("$READER" --store "$STORE" ref get nightly)" = "$ROOT_GO" ] || fail "$writer wrote nightly; the other side does not read it back"
  "$WRITER" --store "$STORE" ref list > "$WORK/refs-$writer-own.txt"
  "$READER" --store "$STORE" ref list > "$WORK/refs-$writer-cross.txt"
  [ "$(wc -l < "$WORK/refs-$writer-own.txt")" -eq 2 ] || fail "$writer lists $(cat "$WORK/refs-$writer-own.txt")"
  cmp "$WORK/refs-$writer-own.txt" "$WORK/refs-$writer-cross.txt"
  "$WRITER" --store "$STORE" ls --keys "ref:nightly@docs" > "$WORK/ls-ref-$writer-own.txt"
  "$READER" --store "$STORE" ls --keys "ref:nightly@docs" > "$WORK/ls-ref-$writer-cross.txt"
  cmp "$WORK/ls-ref-$writer-own.txt" "$WORK/ls-ref-$writer-cross.txt"
  # The other side moves the reference optimistically. A stale expectation
  # and "must not exist" are refused and change nothing ...
  if "$READER" --store "$STORE" ref set --expect "$ROOT2" nightly "$ROOT2" 2> /dev/null; then
    fail "a stale --expect moved the reference $writer wrote"
  fi
  if "$READER" --store "$STORE" ref set --expect none nightly "$ROOT2" 2> /dev/null; then
    fail "--expect none overwrote the reference $writer wrote"
  fi
  if "$READER" --store "$STORE" ref rm --expect "$ROOT2" nightly 2> /dev/null; then
    fail "a stale --expect deleted the reference $writer wrote"
  fi
  [ "$("$WRITER" --store "$STORE" ref get nightly)" = "$ROOT_GO" ] || fail "a refused write changed nightly"
  # ... the right one moves it, and both sides see the move.
  "$READER" --store "$STORE" ref set --expect "$ROOT_GO" nightly "$ROOT2"
  [ "$("$WRITER" --store "$STORE" ref get nightly)" = "$ROOT2" ] || fail "$writer does not see the other side's move"
  [ "$("$READER" --store "$STORE" ref get nightly)" = "$ROOT2" ] || fail "the mover does not see its own move"
  # And back again by the writer, expecting what the other side stored.
  "$WRITER" --store "$STORE" ref set --expect "$ROOT2" nightly "$ROOT_GO"
  "$READER" --store "$STORE" ref rm --expect "$ROOT2" other
  # Both sides see the same final state: nightly alone, at the first root.
  "$WRITER" --store "$STORE" ref list > "$WORK/refs-$writer-own.txt"
  "$READER" --store "$STORE" ref list > "$WORK/refs-$writer-cross.txt"
  cmp "$WORK/refs-$writer-own.txt" "$WORK/refs-$writer-cross.txt"
  [ "$(cut -d' ' -f1,2 "$WORK/refs-$writer-own.txt")" = "nightly $ROOT_GO" ] || fail "final references in the $writer store: $(cat "$WORK/refs-$writer-own.txt")"
  "$READER" --store "$STORE" ls "ref:nightly@docs" > /dev/null
done

echo "== commits (identical keys for identical inputs; cross-read)"
DATE=2026-01-02T03:04:05+01:00
MSG=$'second\n\nA body line.'
C1_GO=$("$GO" --store "$WORK/store-go" commit create --author 'Ann <ann@example.com>' --date "$DATE" -m first "$ROOT_GO")
C1_RS=$("$RS" --store "$WORK/store-rs" commit create --author 'Ann <ann@example.com>' --date "$DATE" -m first "$ROOT_RS")
echo "   go:   $C1_GO"
echo "   rust: $C1_RS"
[ "$C1_GO" = "$C1_RS" ] || { echo "FAIL: root commit keys differ" >&2; exit 1; }
# A child commit of a subdirectory, with a distinct committer and a body.
C2_GO=$("$GO" --store "$WORK/store-go" commit create --ref main --author 'Ann <ann@example.com>' --committer Bob \
  --date "$DATE" --parent "$C1_GO" -m "$MSG" "$ROOT_GO/docs")
C2_RS=$("$RS" --store "$WORK/store-rs" commit create --ref main --author 'Ann <ann@example.com>' --committer Bob \
  --date "$DATE" --parent "$C1_RS" -m "$MSG" "$ROOT_RS/docs")
[ "$C2_GO" = "$C2_RS" ] || { echo "FAIL: child commit keys differ ($C2_GO vs $C2_RS)" >&2; exit 1; }
"$GO" --store "$WORK/store-go" commit show "$C2_GO" > "$WORK/show-go-own.txt"
"$RS" --store "$WORK/store-go" commit show "$C2_GO" > "$WORK/show-rs-cross.txt"
cmp "$WORK/show-go-own.txt" "$WORK/show-rs-cross.txt"
"$RS" --store "$WORK/store-rs" commit show "$C2_RS" > "$WORK/show-rs-own.txt"
"$GO" --store "$WORK/store-rs" commit show "$C2_RS" > "$WORK/show-go-cross.txt"
cmp "$WORK/show-rs-own.txt" "$WORK/show-go-cross.txt"
cmp "$WORK/show-go-own.txt" "$WORK/show-rs-own.txt"
# A commit stands for its tree: list through the commit the OTHER side wrote.
"$GO" --store "$WORK/store-rs" ls --keys "$C2_RS/deep" > "$WORK/ls-commit-go.txt"
"$RS" --store "$WORK/store-go" ls --keys "$C2_GO/deep" > "$WORK/ls-commit-rs.txt"
cmp "$WORK/ls-commit-go.txt" "$WORK/ls-commit-rs.txt"
# commit create --ref wrote a reference; each side resolves the OTHER's.
[ "$("$RS" --store "$WORK/store-rs" ref get main)" = "$C2_RS" ]
[ "$("$GO" --store "$WORK/store-rs" ref get main)" = "$C2_RS" ]
[ "$("$RS" --store "$WORK/store-go" ref get main)" = "$C2_GO" ]

echo "== two processes, two implementations, one store (concurrent ingest)"
# Both CLIs write into the same store directory at the same time: each owns
# an active segment of its own, indexes it in a sidecar the other reads, and
# sees the other's objects (architecture/packstore.md).
T2="$WORK/tree2"
mkdir -p "$T2/sub"
head -c 2000000 /dev/urandom > "$T2/other.bin"
printf 'second tree\n' > "$T2/sub/note.txt"
SHARED="$WORK/store-shared"
"$GO" --store "$SHARED" ingest "$T" > "$WORK/shared-root1" &
GO_PID=$!
ROOT2=$("$RS" --store "$SHARED" ingest "$T2")
wait "$GO_PID"
ROOT1=$(cat "$WORK/shared-root1")
[ "$ROOT1" = "$ROOT_GO" ] || { echo "FAIL: concurrent go ingest gave $ROOT1" >&2; exit 1; }
echo "   active segments afterwards: $(find "$SHARED/packstore" -name '*.seg.active' | wc -l | tr -d ' ')"
"$GO" --store "$SHARED" export -o "$WORK/shared1-go.tar" "$ROOT1"
"$RS" --store "$SHARED" export -o "$WORK/shared1-rs.tar" "$ROOT1"
cmp "$WORK/go-from-go.tar" "$WORK/shared1-go.tar"
cmp "$WORK/go-from-go.tar" "$WORK/shared1-rs.tar"
"$GO" --store "$SHARED" export -o "$WORK/shared2-go.tar" "$ROOT2"
"$RS" --store "$SHARED" export -o "$WORK/shared2-rs.tar" "$ROOT2"
cmp "$WORK/shared2-go.tar" "$WORK/shared2-rs.tar"
# And in turn: each continues the segment, and the sidecar, the other left.
printf 'third\n' > "$T2/sub/third.txt"
ROOT3_RS=$("$RS" --store "$SHARED" ingest "$T2")
printf 'fourth\n' > "$T2/sub/fourth.txt"
ROOT4_GO=$("$GO" --store "$SHARED" ingest "$T2")
"$GO" --store "$SHARED" ls --keys "$ROOT3_RS/sub" > "$WORK/ls3-go.txt"
"$RS" --store "$SHARED" ls --keys "$ROOT3_RS/sub" > "$WORK/ls3-rs.txt"
cmp "$WORK/ls3-go.txt" "$WORK/ls3-rs.txt"
"$GO" --store "$SHARED" ls --keys "$ROOT4_GO/sub" > "$WORK/ls4-go.txt"
"$RS" --store "$SHARED" ls --keys "$ROOT4_GO/sub" > "$WORK/ls4-rs.txt"
cmp "$WORK/ls4-go.txt" "$WORK/ls4-rs.txt"

echo "== repairing packs across implementations"
(cd "$RS_REPO" && cargo build -q --example repair-interop)
(cd "$GO_REPO" && go build -o "$WORK/repair-go" "$RS_REPO/interop/repair.go")
RS_REPAIR="$RS_REPO/target/debug/examples/repair-interop"
for producer in rust go; do
  if [ "$producer" = rust ]; then
    CREATE="$RS_REPAIR"
    REPAIR="$WORK/repair-go"
  else
    CREATE="$WORK/repair-go"
    REPAIR="$RS_REPAIR"
  fi
  STORE="$WORK/store-repair-$producer"
  "$CREATE" create "$STORE"
  python3 - "$STORE/0000000000000001.seg" <<'PYREPAIR'
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
record = bytearray(path.read_bytes())
record[8] ^= 0x40  # Damage the first record header, preserving the footer.
path.write_bytes(record)
PYREPAIR
  "$REPAIR" repair "$STORE"
  "$CREATE" check "$STORE"
done

echo "OK: all interop checks passed"
