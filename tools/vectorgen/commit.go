package main

import (
	"bytes"
	"encoding/hex"
	"fmt"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/amber-store/core/commit"
	"github.com/amber-store/core/key"
)

type commitIdentity struct {
	Name     string `json:"name"`
	Email    string `json:"email"`
	When     string `json:"when"` // decimal int64, ns
	TZOffset int    `json:"tz_offset"`
}

type commitCase struct {
	Name                string         `json:"name"`
	Tree                string         `json:"tree"`
	Parents             []string       `json:"parents"`
	Author              commitIdentity `json:"author"`
	Committer           commitIdentity `json:"committer"`
	Message             string         `json:"message"`
	SignatureHex        string         `json:"signature_hex,omitempty"`
	PublicKeyHex        string         `json:"public_key_hex,omitempty"`
	BytesHex            string         `json:"bytes_hex"`
	Key                 string         `json:"key"`
	SignaturePayloadHex string         `json:"signature_payload_hex"`
}

type commitFile struct {
	Cases []commitCase `json:"cases"`
}

// genCommit writes commit.json: canonical Commit encodings with their keys
// and signature payloads.
func genCommit(outDir string) error {
	emptyDir, err := key.New(key.DirLeaf, 1, []byte{0x80})
	if err != nil {
		return err
	}
	dirNode, err := key.New(key.DirNode, 4096, smData(30, 64))
	if err != nil {
		return err
	}
	ann := commit.Identity{Name: "Ann", Email: "ann@example.com", When: 1700000000000000000, TZOffset: 120}
	bob := commit.Identity{Name: "Bob", Email: "", When: 1700000000000000001, TZOffset: -300}

	type named struct {
		name string
		c    commit.Commit
	}
	var cases []named
	keyOf := func(c commit.Commit) key.Key {
		k, _, err := c.Object()
		if err != nil {
			panic(err)
		}
		return k
	}
	rootA := commit.Commit{Tree: emptyDir, Author: ann, Committer: ann, Message: "a"}
	rootB := commit.Commit{Tree: emptyDir, Author: ann, Committer: ann, Message: "b"}
	merge := commit.Commit{Tree: emptyDir, Parents: []key.Key{keyOf(rootA), keyOf(rootB)}, Author: ann, Committer: bob, Message: "merge\n"}
	cases = append(cases,
		named{"root a", rootA}, named{"root b", rootB},
		// The vector annotated in architecture/commits.md.
		named{"merge", merge},
	)

	signed := merge
	signed.Signature, signed.PublicKey = smData(31, 64), smData(32, 68)
	cases = append(cases, named{"signed shape: opaque dummy signature + public key", signed})

	pubOnly := merge
	pubOnly.PublicKey = smData(32, 68)
	cases = append(cases, named{"public key without signature", pubOnly})

	// 24 parents: the array head grows a length byte; a DirNode tree; a
	// message past the one-byte string length; unicode identities; a
	// negative timestamp and the extreme offsets.
	var many []key.Key
	for i := 0; i < 24; i++ {
		many = append(many, keyOf(commit.Commit{Tree: emptyDir, Author: ann, Committer: ann, Message: "p" + strconv.Itoa(i)}))
	}
	cases = append(cases, named{"octopus, unicode, boundaries", commit.Commit{
		Tree:      dirNode,
		Parents:   many,
		Author:    commit.Identity{Name: "名前 ✓", Email: "π@example.com", When: -1, TZOffset: 1439},
		Committer: commit.Identity{Name: strings.Repeat("n", 1024), Email: strings.Repeat("e", 1024), When: 0, TZOffset: -1439},
		Message:   "subject ✓\n\n" + strings.Repeat("body line\n", 30),
	}})
	cases = append(cases, named{"empty message, no email", commit.Commit{
		Tree: emptyDir, Author: bob, Committer: bob, Message: "",
	}})

	out := commitFile{Cases: make([]commitCase, 0, len(cases))}
	for _, n := range cases {
		k, enc, err := n.c.Object()
		if err != nil {
			return fmt.Errorf("%q: %w", n.name, err)
		}
		dec, err := commit.Decode(enc)
		if err != nil {
			return fmt.Errorf("%q: decode round-trip: %w", n.name, err)
		}
		reEnc, err := dec.Encode()
		if err != nil {
			return fmt.Errorf("%q: re-encode: %w", n.name, err)
		}
		if !bytes.Equal(reEnc, enc) {
			return fmt.Errorf("%q: round-trip bytes differ", n.name)
		}
		payload, err := n.c.SignaturePayload()
		if err != nil {
			return fmt.Errorf("%q: signature payload: %w", n.name, err)
		}
		ident := func(id commit.Identity) commitIdentity {
			return commitIdentity{Name: id.Name, Email: id.Email, When: strconv.FormatInt(id.When, 10), TZOffset: id.TZOffset}
		}
		cc := commitCase{
			Name: n.name, Tree: n.c.Tree.String(), Parents: []string{},
			Author: ident(n.c.Author), Committer: ident(n.c.Committer), Message: n.c.Message,
			BytesHex: hex.EncodeToString(enc), Key: k.String(), SignaturePayloadHex: hex.EncodeToString(payload),
		}
		for _, p := range n.c.Parents {
			cc.Parents = append(cc.Parents, p.String())
		}
		if len(n.c.Signature) > 0 {
			cc.SignatureHex = hex.EncodeToString(n.c.Signature)
		}
		if len(n.c.PublicKey) > 0 {
			cc.PublicKeyHex = hex.EncodeToString(n.c.PublicKey)
		}
		out.Cases = append(out.Cases, cc)
	}
	return writeJSON(filepath.Join(outDir, "commit.json"), out)
}
