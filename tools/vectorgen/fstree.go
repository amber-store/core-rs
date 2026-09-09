package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"sort"

	"github.com/amber-store/core/cborx"
	"github.com/amber-store/core/chunkers"
	"github.com/amber-store/core/fstree"
	"github.com/amber-store/core/key"
)

// goldenItemBits and goldenXattrInlineMax are the golden tree's chunking
// parameters (the ingest defaults: DefaultItemBits / DefaultXattrInlineMax).
const (
	goldenItemBits       = 7
	goldenXattrInlineMax = 256
)

// goldenTree is the fully built golden fstree: the root key and the
// deduplicated object set.
type goldenTree struct {
	root   key.Key
	objs   map[key.Key][]byte
	sorted []key.Key // manifest order: ascending key hex (== bytewise)
}

// get is a fstree Getter over the built objects.
func (t *goldenTree) get(k key.Key) ([]byte, error) {
	b, ok := t.objs[k]
	if !ok {
		return nil, fmt.Errorf("object %s not in golden tree", k)
	}
	return b, nil
}

// treeBuilder accumulates emitted objects (deduplicated by key).
type treeBuilder struct {
	ic   chunkers.ItemChunker
	objs map[key.Key][]byte
}

func (tb *treeBuilder) emit(o fstree.Object) error {
	if prev, ok := tb.objs[o.Key]; ok {
		if !bytes.Equal(prev, o.Bytes) {
			return fmt.Errorf("key %s emitted with differing bytes", o.Key)
		}
		return nil
	}
	tb.objs[o.Key] = o.Bytes
	return nil
}

// buildFile mirrors ingest's driver.buildFile: split content with the default
// ultracdc parameters, encode each chunk as a Blob, feed the blob keys through
// a file IndexBuilder (which returns a single blob's key unwrapped, and builds
// FileNode levels above multiple blobs). An empty file is a single empty Blob.
func (tb *treeBuilder) buildFile(content []byte) (key.Key, error) {
	ib := fstree.NewFileIndexBuilder(tb.ic)
	saw := false
	err := chunkers.SplitBytes(bytes.NewReader(content), nil, func(chunk []byte) error {
		saw = true
		obj, err := fstree.EncodeBlob(chunk)
		if err != nil {
			return err
		}
		if err := tb.emit(obj); err != nil {
			return err
		}
		return ib.AddChild(tb.emit, obj.Key, nil)
	})
	if err != nil {
		return key.Key{}, err
	}
	if !saw {
		obj, err := fstree.EncodeBlob([]byte{})
		if err != nil {
			return key.Key{}, err
		}
		if err := tb.emit(obj); err != nil {
			return key.Key{}, err
		}
		if err := ib.AddChild(tb.emit, obj.Key, nil); err != nil {
			return key.Key{}, err
		}
	}
	return ib.Finish(tb.emit)
}

// buildDir feeds entries (sorted bytewise by name here) through a DirBuilder
// and returns the directory's root key.
func (tb *treeBuilder) buildDir(entries []fstree.Entry) (key.Key, error) {
	sort.Slice(entries, func(i, j int) bool {
		return bytes.Compare(entries[i].Name, entries[j].Name) < 0
	})
	db := fstree.NewDirBuilder(tb.ic)
	for _, e := range entries {
		if err := db.AddEntry(tb.emit, e); err != nil {
			return key.Key{}, err
		}
	}
	return db.Finish(tb.emit)
}

// setXattrs applies ingest's inline-vs-spill rule (driver.buildEntry): the
// canonical CBOR xattr map stays inline iff its encoding is <= 256 bytes,
// otherwise it is emitted as an XattrSet object referenced by key 9.
func (tb *treeBuilder) setXattrs(e *fstree.Entry, xattrs map[string][]byte) error {
	if len(xattrs) == 0 {
		return nil
	}
	enc := cborx.EncodeXattrs(xattrs)
	if len(enc) <= goldenXattrInlineMax {
		e.XattrsIn = enc
		return nil
	}
	obj, err := fstree.EncodeXattrSet(xattrs)
	if err != nil {
		return err
	}
	if err := tb.emit(obj); err != nil {
		return err
	}
	e.XattrsKey = obj.Key[:]
	return nil
}

// mt is the mtime helper from VECTORS.md: s*1e9 + ns nanoseconds.
func mt(s, ns int64) int64 { return s*1_000_000_000 + ns }

// buildGoldenTree constructs the VECTORS.md golden tree in memory through the
// public fstree builder APIs.
func buildGoldenTree() (*goldenTree, error) {
	tb := &treeBuilder{
		ic:   chunkers.NewItemChunker(goldenItemBits),
		objs: make(map[key.Key][]byte),
	}

	// fileEntry builds a regular file's content and entry. Unstated metadata
	// fields are zero (in particular mtime 0 for the root files).
	fileEntry := func(name string, content Payload, mode, uid, gid uint64, mtime int64, xattrs map[string][]byte) (fstree.Entry, error) {
		ck, err := tb.buildFile(content.Materialize())
		if err != nil {
			return fstree.Entry{}, err
		}
		e := fstree.Entry{
			Name: []byte(name), Mode: mode, UID: uid, GID: gid, Mtime: mtime,
			ContentKey: ck[:],
		}
		if err := tb.setXattrs(&e, xattrs); err != nil {
			return fstree.Entry{}, err
		}
		return e, nil
	}

	// sub: special files and metadata edge cases (uid 0, gid 0 unless stated).
	var subEntries []fstree.Entry
	subEntries = append(subEntries, fstree.Entry{
		Name: []byte("ln"), Mode: 0o120777, Mtime: mt(1600000000, 500),
		LinkTarget: []byte("../small.txt"),
	})
	subEntries = append(subEntries, fstree.Entry{
		Name: []byte("fifo"), Mode: 0o10644, Mtime: mt(1600000001, 0),
	})
	subEntries = append(subEntries, fstree.Entry{
		Name: []byte("sock"), Mode: 0o140644, Mtime: mt(1600000002, 0),
	})
	subEntries = append(subEntries, fstree.Entry{
		Name: []byte("chr"), Mode: 0o20644, Mtime: mt(1600000003, 0),
		Rdev: []uint64{1, 3},
	})
	subEntries = append(subEntries, fstree.Entry{
		Name: []byte("blk"), Mode: 0o60644, Mtime: mt(1600000004, 0),
		Rdev: []uint64{259, 0},
	})
	e, err := fileEntry("xattr-inline", SM(6, 50), 0o100600, 501, 20, mt(1500000000, 123456789),
		map[string][]byte{"user.a": smData(7, 5), "user.b": smData(8, 100)})
	if err != nil {
		return nil, err
	}
	if len(e.XattrsIn) == 0 || len(e.XattrsKey) != 0 {
		return nil, fmt.Errorf("xattr-inline: expected inline xattrs")
	}
	subEntries = append(subEntries, e)
	e, err = fileEntry("xattr-spilled", SM(9, 50), 0o100644, 0, 0, mt(1500000001, 0),
		map[string][]byte{"user.big": smData(10, 400)})
	if err != nil {
		return nil, err
	}
	if len(e.XattrsKey) != key.Size || len(e.XattrsIn) != 0 {
		return nil, fmt.Errorf("xattr-spilled: expected spilled xattrs")
	}
	subEntries = append(subEntries, e)
	e, err = fileEntry("old", SM(11, 10), 0o100644, 0, 0, mt(-1, 999999999), nil)
	if err != nil {
		return nil, err
	}
	subEntries = append(subEntries, e)
	e, err = fileEntry("setuid", SM(12, 10), 0o104755, 4294967294, 4294967294, 0, nil)
	if err != nil {
		return nil, err
	}
	subEntries = append(subEntries, e)
	subRoot, err := tb.buildDir(subEntries)
	if err != nil {
		return nil, fmt.Errorf("sub: %w", err)
	}

	// bigdir: 3000 regular files e%06d, content data(1000+i, i mod 50).
	bigdirEntries := make([]fstree.Entry, 0, 3000)
	for i := 0; i < 3000; i++ {
		e, err := fileEntry(fmt.Sprintf("e%06d", i), SM(uint64(1000+i), i%50),
			0o100644, 1000, 1000, mt(1700000000+int64(i), int64(i)), nil)
		if err != nil {
			return nil, err
		}
		bigdirEntries = append(bigdirEntries, e)
	}
	bigdirRoot, err := tb.buildDir(bigdirEntries)
	if err != nil {
		return nil, fmt.Errorf("bigdir: %w", err)
	}

	// Root entries (files mode 0o100644, dirs 0o40755, uid/gid 1000/1000,
	// unstated mtimes 0).
	var rootEntries []fstree.Entry
	rootFiles := []struct {
		name    string
		content Payload
	}{
		{"empty", SM(0, 0)},
		{"small.txt", SM(1, 100)},
		{"medium.bin", SM(2, 30000)},
		{"big.bin", SM(3, 5242880)},
		{"constant.dat", Const(0xAA, 300000)},
		{"A-upper", SM(4, 10)},
		{"\xc3\xa9-utf8", SM(5, 10)},
	}
	for _, f := range rootFiles {
		e, err := fileEntry(f.name, f.content, 0o100644, 1000, 1000, 0, nil)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", f.name, err)
		}
		rootEntries = append(rootEntries, e)
	}
	rootEntries = append(rootEntries, fstree.Entry{
		Name: []byte("sub"), Mode: 0o40755, UID: 1000, GID: 1000,
		ContentKey: subRoot[:],
	})
	rootEntries = append(rootEntries, fstree.Entry{
		Name: []byte("bigdir"), Mode: 0o40755, UID: 1000, GID: 1000,
		ContentKey: bigdirRoot[:],
	})
	root, err := tb.buildDir(rootEntries)
	if err != nil {
		return nil, fmt.Errorf("root: %w", err)
	}

	sorted := make([]key.Key, 0, len(tb.objs))
	for k := range tb.objs {
		sorted = append(sorted, k)
	}
	sort.Slice(sorted, func(i, j int) bool {
		return bytes.Compare(sorted[i][:], sorted[j][:]) < 0
	})
	return &goldenTree{root: root, objs: tb.objs, sorted: sorted}, nil
}

type manifestObject struct {
	Key  string `json:"key"`
	Size int    `json:"size"`
}

type fstreeManifest struct {
	Root    string           `json:"root"`
	Objects []manifestObject `json:"objects"`
}

// genFstree builds the golden tree and writes fstree/manifest.json and
// fstree/objects.bin.
func genFstree(outDir string) (*goldenTree, error) {
	tree, err := buildGoldenTree()
	if err != nil {
		return nil, err
	}
	dir := filepath.Join(outDir, "fstree")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, err
	}

	man := fstreeManifest{
		Root:    tree.root.String(),
		Objects: make([]manifestObject, 0, len(tree.sorted)),
	}
	var bin bytes.Buffer
	for _, k := range tree.sorted {
		b := tree.objs[k]
		man.Objects = append(man.Objects, manifestObject{Key: k.String(), Size: len(b)})
		bin.Write(k[:])
		var lenBuf [8]byte
		binary.BigEndian.PutUint64(lenBuf[:], uint64(len(b)))
		bin.Write(lenBuf[:])
		bin.Write(b)
	}
	if err := writeJSON(filepath.Join(dir, "manifest.json"), man); err != nil {
		return nil, err
	}
	if err := os.WriteFile(filepath.Join(dir, "objects.bin"), bin.Bytes(), 0o644); err != nil {
		return nil, err
	}
	return tree, nil
}
