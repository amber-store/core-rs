// Run from the Go repository through interop/check.sh.
package main

import (
	"bytes"
	"context"
	"os"

	"github.com/amber-store/core/key"
	"github.com/amber-store/core/packstore"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func main() {
	store, err := packstore.Open(os.Args[2], packstore.WithSegmentSize(1))
	must(err)
	for _, data := range [][]byte{[]byte("repair target"), []byte("unrelated survivor")} {
		k, err := key.New(key.Blob, uint64(len(data)), data)
		must(err)
		switch os.Args[1] {
		case "create":
			must(store.Put(k, data))
		case "repair":
			must(store.PutVerified(k, data))
		case "check":
			got, err := store.Get(k)
			must(err)
			if !bytes.Equal(got, data) {
				panic("repaired content mismatch")
			}
		default:
			panic("expected create, repair, or check")
		}
	}
	must(store.Verify(context.Background()))
	must(store.Close())
}
