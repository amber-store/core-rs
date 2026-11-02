package main

import (
	"bytes"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/amber-store/core/amberpack"
	"github.com/amber-store/core/gitobj"
	"github.com/amber-store/core/key"
	"github.com/amber-store/core/packstore"
)

type gitLink struct {
	Key  string `json:"key"`
	Leaf bool   `json:"leaf"`
}

type gitCase struct {
	Name    string `json:"name"`
	DataHex string `json:"data_hex"`
	// Key is what the payload hashes to; KeyError the error if it has no key.
	Key      string `json:"key,omitempty"`
	KeyError string `json:"key_error,omitempty"`
	// Links is what a walk follows; LinksError the error if it cannot read them.
	Links      []gitLink `json:"links"`
	LinksError string    `json:"links_error,omitempty"`
}

type gitFile struct {
	Cases []gitCase `json:"cases"`
}

// genGitobj writes gitobj.json: payloads of every kind, good and bad, with the
// key, the links and the error text the Go implementation gives each.
func genGitobj(outDir string) error {
	const (
		oidA = "ce013625030ba8dba906f756967f9e9ca394464a"
		oidB = "49dff37cdebcd729bcfa301ce7b8b5b8e57da82e"
		oidC = "15681690adcc0ebdfbf7a6ddc67a410536538e08"
	)
	unhex := func(s string) string {
		b, err := hex.DecodeString(s)
		if err != nil {
			panic(err)
		}
		return string(b)
	}
	a, b := unhex(oidA), unhex(oidB)
	sig := "tree " + oidB + "\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\n"
	framed := func(k gitobj.Kind, body string) []byte { return gitobj.Encode(k, []byte(body)) }

	cases := []struct {
		name string
		data []byte
	}{
		{"empty blob", framed(gitobj.Blob, "")},
		{"blob", framed(gitobj.Blob, "hello\n")},
		{"blob reading like a commit", framed(gitobj.Blob, sig)},
		{"empty tree", framed(gitobj.Tree, "")},
		{"tree", framed(gitobj.Tree, "40000 dir\x00"+b+"100644 file\x00"+a)},
		{"tree: padded mode", framed(gitobj.Tree, "040000 dir\x00"+b)},
		{"tree: group-writable mode", framed(gitobj.Tree, "100664 f\x00"+a)},
		{"tree: executable", framed(gitobj.Tree, "100755 f\x00"+a)},
		{"tree: symlink", framed(gitobj.Tree, "120000 l\x00"+a)},
		{"tree: gitlink", framed(gitobj.Tree, "160000 m\x00"+a)},
		{"tree: zero mode", framed(gitobj.Tree, "0 x\x00"+a)},
		{"tree: unknown file type", framed(gitobj.Tree, "60000 x\x00"+a)},
		{"tree: mode shifts bits out", framed(gitobj.Tree, "7777777777777777100644 x\x00"+a)},
		{"tree: name with spaces", framed(gitobj.Tree, "100644 a b\nc\x00"+a)},
		{"tree: unsorted, duplicated", framed(gitobj.Tree, "100644 z\x00"+a+"100644 a\x00"+a+"100644 a\x00"+a)},
		{"tree: no space", framed(gitobj.Tree, "100644")},
		{"tree: empty mode", framed(gitobj.Tree, " x\x00"+a)},
		{"tree: mode not octal", framed(gitobj.Tree, "100648 x\x00"+a)},
		{"tree: no NUL", framed(gitobj.Tree, "100644 x")},
		{"tree: empty name", framed(gitobj.Tree, "100644 \x00"+a)},
		{"tree: short id", framed(gitobj.Tree, "100644 x\x00"+a[:19])},
		{"tree: trailing bytes", framed(gitobj.Tree, "100644 x\x00"+a+"1")},
		{"commit", framed(gitobj.Commit, sig+"\nm\n")},
		{"commit: parents", framed(gitobj.Commit, "tree "+oidB+"\nparent "+oidC+"\nparent "+oidA+"\nauthor a <a@b> 1 +0000\n\nm\n")},
		{"commit: tree line alone", framed(gitobj.Commit, "tree "+oidB+"\n")},
		{"commit: uppercase hex", framed(gitobj.Commit, "tree "+strings.ToUpper(oidB)+"\n")},
		{"commit: later headers are no links", framed(gitobj.Commit, sig+"mergetag object "+oidA+"\n type commit\n\nparent "+oidC+"\n")},
		{"commit: empty", framed(gitobj.Commit, "")},
		{"commit: no tree", framed(gitobj.Commit, "author a <a@b> 1 +0000\n")},
		{"commit: short tree", framed(gitobj.Commit, "tree "+oidB[:39]+"\n")},
		{"commit: tree not hex", framed(gitobj.Commit, "tree "+strings.Repeat("z", 40)+"\n")},
		{"commit: tree no newline", framed(gitobj.Commit, "tree "+oidB)},
		{"commit: short final parent line", framed(gitobj.Commit, "tree "+oidB+"\nparent abc\n")},
		{"commit: final parent line without a newline", framed(gitobj.Commit, "tree "+oidB+"\nparent "+oidC)},
		{"commit: parent line, no id", framed(gitobj.Commit, "tree "+oidB+"\nparent \n")},
		{"commit: parent line long enough, not one", framed(gitobj.Commit, "tree "+oidB+"\nparent abc\nauthor a <a@b> 1 +0000\ncommitter a <a@b> 1 +0000\n\nm\n")},
		{"commit: parent line too long", framed(gitobj.Commit, "tree "+oidB+"\nparent "+oidC+"0\n")},
		{"tag of a commit", framed(gitobj.Tag, "object "+oidC+"\ntype commit\ntag v\n\nm\n")},
		{"tag of a tree", framed(gitobj.Tag, "object "+oidB+"\ntype tree\ntag v\n\nm\n")},
		{"tag of a tag", framed(gitobj.Tag, "object "+oidC+"\ntype tag\ntag v\n\nm\n")},
		{"tag of a blob", framed(gitobj.Tag, "object "+oidA+"\ntype blob\ntag v\n\nm\n")},
		{"tag: no object", framed(gitobj.Tag, "type commit\n")},
		{"tag: no type", framed(gitobj.Tag, "object "+oidC+"\ntag v\n")},
		{"tag: unknown type", framed(gitobj.Tag, "object "+oidC+"\ntype file\n")},
		{"tag: type no newline", framed(gitobj.Tag, "object "+oidC+"\ntype commit")},
		{"tag: type ends at a NUL", framed(gitobj.Tag, "object "+oidC+"\ntype commit\x00junk\ntag v\n\nm\n")},
		{"tag: type empty before the NUL", framed(gitobj.Tag, "object "+oidC+"\ntype \x00commit\n")},
		{"frame: empty", nil},
		{"frame: no space", []byte("blob")},
		{"frame: no NUL", []byte("blob 3")},
		{"frame: unknown kind", []byte("file 0\x00")},
		{"frame: empty size", []byte("blob \x00")},
		{"frame: leading zero", []byte("blob 01\x00x")},
		{"frame: size too large", []byte("blob 2\x00x")},
		{"frame: size too small", []byte("blob 1\x00xy")},
		{"frame: size overflows", []byte("blob 18446744073709551616\x00")},
		{"shattered prefix", shatteredPrefix()},
	}

	var f gitFile
	for _, c := range cases {
		g := gitCase{Name: c.name, DataHex: hex.EncodeToString(c.data), Links: []gitLink{}}
		k, err := gitobj.KeyOf(c.data)
		if err != nil {
			g.KeyError = err.Error()
		} else {
			g.Key = k.String()
		}
		links, err := gitobj.Links(c.data)
		if err != nil {
			g.LinksError = err.Error()
		}
		for _, l := range links {
			g.Links = append(g.Links, gitLink{Key: l.Key.String(), Leaf: l.Leaf})
		}
		f.Cases = append(f.Cases, g)
	}
	return writeJSON(filepath.Join(outDir, "gitobj.json"), f)
}

// shatteredPrefix is the first 320 bytes of shattered-1.pdf: the prefix and the
// two blocks of the first public SHA-1 collision.
func shatteredPrefix() []byte {
	b, err := hex.DecodeString("" +
		"255044462d312e330a25e2e3cfd30a0a0a312030206f626a0a3c3c2f57696474" +
		"682032203020522f4865696768742033203020522f547970652034203020522f" +
		"537562747970652035203020522f46696c7465722036203020522f436f6c6f72" +
		"53706163652037203020522f4c656e6774682038203020522f42697473506572" +
		"436f6d706f6e656e7420383e3e0a73747265616d0affd8fffe00245348412d31" +
		"20697320646561642121212121852fec092339759c39b1a1c63c4c97e1fffe01" +
		"7346dc9166b67e118f029ab621b2560ff9ca67cca8c7f85ba84c79030c2b3de2" +
		"18f86db3a90901d5df45c14f26fedfb3dc38e96ac22fe7bd728f0e45bce046d2" +
		"3c570feb141398bb552ef5a0a82be331fea48037b8b5d71f0e332edf93ac3500" +
		"eb4ddc0decc1a864790c782c76215660dd309791d06bd0af3f98cda4bc4629b1")
	if err != nil {
		panic(err)
	}
	return b
}

type gitSegObject struct {
	Key     string `json:"key"`
	DataHex string `json:"data_hex"`
}

type gitSegments struct {
	SegmentSize int            `json:"segment_size"`
	Root        string         `json:"root"`
	Objects     []gitSegObject `json:"objects"` // every object the root reaches
	Unreached   []string       `json:"unreached"`
}

// genSegmentsGit writes segments_go_git/, a store holding a small git history
// with zstd records: sealed segments and an unsealed active one, all at format
// version 4, plus its manifest.
func genSegmentsGit(outDir string) error {
	const segmentSize = 4096
	dir := filepath.Join(outDir, "segments_go_git")
	st, err := packstore.Open(dir, packstore.WithSegmentSize(segmentSize), packstore.WithSync(false),
		packstore.WithCompression(amberpack.Compression{Algorithm: amberpack.Zstd}))
	if err != nil {
		return err
	}
	defer st.Close()

	man := gitSegments{SegmentSize: segmentSize}
	put := func(kind gitobj.Kind, body []byte) (key.Key, error) {
		data := gitobj.Encode(kind, body)
		k, err := gitobj.KeyOf(data)
		if err != nil {
			return key.Key{}, err
		}
		if err := st.PutVerified(k, data); err != nil {
			return key.Key{}, err
		}
		return k, nil
	}
	reached := func(kind gitobj.Kind, body []byte) (key.Key, error) {
		k, err := put(kind, body)
		if err != nil {
			return key.Key{}, err
		}
		man.Objects = append(man.Objects, gitSegObject{Key: k.String(), DataHex: hex.EncodeToString(gitobj.Encode(kind, body))})
		return k, nil
	}
	oid := func(k key.Key) string { return k.String()[:2*key.GitSHA1Size] }

	var head key.Key
	for i := range 8 {
		blob, err := reached(gitobj.Blob, smData(uint64(9000+i), 1500))
		if err != nil {
			return err
		}
		sum, _ := blob.GitSHA1Sum()
		sub, err := reached(gitobj.Tree, []byte("100644 f\x00"+string(sum[:])))
		if err != nil {
			return err
		}
		subSum, _ := sub.GitSHA1Sum()
		tree, err := reached(gitobj.Tree, []byte("40000 d\x00"+string(subSum[:])+"160000 m\x00"+strings.Repeat("\x11", key.GitSHA1Size)))
		if err != nil {
			return err
		}
		var c bytes.Buffer
		fmt.Fprintf(&c, "tree %s\n", oid(tree))
		if i > 0 {
			fmt.Fprintf(&c, "parent %s\n", oid(head))
		}
		fmt.Fprintf(&c, "author a <a@b> %d +0000\ncommitter a <a@b> %d +0000\n\ncommit %d\n", i, i, i)
		if head, err = reached(gitobj.Commit, c.Bytes()); err != nil {
			return err
		}
		dead, err := put(gitobj.Blob, []byte(fmt.Sprintf("reached by nothing %d\n", i)))
		if err != nil {
			return err
		}
		man.Unreached = append(man.Unreached, dead.String())
	}
	root, err := reached(gitobj.Tag, []byte("object "+oid(head)+"\ntype commit\ntag v1\n\nm\n"))
	if err != nil {
		return err
	}
	man.Root = root.String()
	if err := st.Close(); err != nil {
		return err
	}
	// gc.lock is every open's and not part of the fixture.
	if err := os.Remove(filepath.Join(dir, "gc.lock")); err != nil {
		return err
	}
	paths, err := filepath.Glob(filepath.Join(dir, "*.seg*"))
	if err != nil {
		return err
	}
	var sealed, active int
	for _, p := range paths {
		switch {
		case strings.HasSuffix(p, ".seg"):
			sealed++
		case strings.HasSuffix(p, ".seg.active"):
			active++
		default:
			continue
		}
		b, err := os.ReadFile(p)
		if err != nil {
			return err
		}
		if b[7] != 4 {
			return fmt.Errorf("%s: at version %d, want 4", p, b[7])
		}
	}
	if sealed == 0 || active != 1 {
		return fmt.Errorf("%d sealed and %d active segments, want some sealed and one active", sealed, active)
	}
	return writeJSON(filepath.Join(dir, "manifest.json"), man)
}
