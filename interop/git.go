// Run from the Go repository through interop/check.sh. The same small history
// as examples/git-interop.rs: both must give it the same keys.
package main

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"slices"

	"github.com/amber-store/core/fstree"
	"github.com/amber-store/core/gitobj"
	"github.com/amber-store/core/key"
	"github.com/amber-store/core/packstore"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}

type object struct {
	key  key.Key
	data []byte
}

func make1(kind gitobj.Kind, body string) object {
	data := gitobj.Encode(kind, []byte(body))
	k, err := gitobj.KeyOf(data)
	must(err)
	return object{k, data}
}

func raw(o object) string {
	sum, ok := o.key.GitSHA1Sum()
	if !ok {
		panic("not a git key")
	}
	return string(sum[:])
}

func oid(o object) string { return fmt.Sprintf("%x", raw(o)) }

// history returns what the root reaches, the root last, and what it does not.
func history() (reached []object, dangling object) {
	blob := make1(gitobj.Blob, "hello\n")
	sub := make1(gitobj.Tree, "100644 f\x00"+raw(blob))
	tree := make1(gitobj.Tree, "100755 run\x00"+raw(blob)+"120000 sym\x00"+raw(blob)+"160000 mod\x00"+raw(blob)+"40000 z\x00"+raw(sub))
	first := make1(gitobj.Commit, "tree "+oid(sub)+"\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\n\nfirst\n")
	tip := make1(gitobj.Commit, "tree "+oid(tree)+"\nparent "+oid(first)+"\nauthor a <a@b> 2 +0000\ncommitter a <a@b> 2 +0000\n\nsecond\n")
	tag := make1(gitobj.Tag, "object "+oid(tip)+"\ntype commit\ntag v1\n\nm\n")
	return []object{blob, sub, tree, first, tip, tag}, make1(gitobj.Blob, "reached by nothing\n")
}

func main() {
	reached, dangling := history()
	root := reached[len(reached)-1]
	switch os.Args[1] {
	case "keys":
		fmt.Println(root.key, reached[0].key, dangling.key)
	case "create":
		store, err := packstore.Open(os.Args[2], packstore.WithSegmentSize(256))
		must(err)
		for _, o := range append(slices.Clone(reached), dangling) {
			must(store.PutVerified(o.key, o.data))
		}
		must(store.Close())
	case "check":
		store, err := packstore.Open(os.Args[2], packstore.WithSegmentSize(256))
		must(err)
		var want []key.Key
		for _, o := range reached {
			got, err := store.Get(o.key)
			must(err)
			if !bytes.Equal(got, o.data) {
				panic("content mismatch")
			}
			want = append(want, o.key)
		}
		visited, err := fstree.CheckComplete(root.key, store.Get, store.Has, 4)
		must(err)
		cmp := func(a, b key.Key) int { return bytes.Compare(a[:], b[:]) }
		slices.SortFunc(want, cmp)
		slices.SortFunc(visited, cmp)
		if !slices.Equal(visited, want) {
			panic("the walk does not reach what the history holds")
		}
		must(store.Verify(context.Background()))
		must(store.Close())
	case "gone":
		store, err := packstore.Open(os.Args[2], packstore.WithSegmentSize(256))
		must(err)
		has, err := store.Has(dangling.key)
		must(err)
		if has {
			panic("the object nothing reaches outlived the collection")
		}
		must(store.Close())
	default:
		panic("expected keys, create, check, or gone")
	}
}
