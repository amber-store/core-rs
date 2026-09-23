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
	ChangeIDHex         string         `json:"change_id_hex,omitempty"`
	ConflictTerms       []string       `json:"conflict_terms,omitempty"`
	ConflictLabels      []string       `json:"conflict_labels,omitempty"`
	BytesHex            string         `json:"bytes_hex"`
	Key                 string         `json:"key"`
	SignaturePayloadHex string         `json:"signature_payload_hex"`
}

type commitFile struct {
	Cases []commitCase `json:"cases"`
}

// fabKey fabricates a key of the given type and length whose hash bytes are
// all fill: the commit codec never fetches a tree, so any canonical key serves.
func fabKey(typ key.Type, length uint64, fill byte) (key.Key, error) {
	var h [32]byte
	for i := range h {
		h[i] = fill
	}
	return key.NewFromHash(typ, length, h)
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

	// What a jj commit carries besides (Go PR #15): a change id, the further
	// terms of a conflicted tree and their labels, identities without a name.
	removed, err := fabKey(key.DirLeaf, 300, 0x11)
	if err != nil {
		return err
	}
	added, err := fabKey(key.DirNode, 70000, 0x22)
	if err != nil {
		return err
	}
	changeID := []byte{0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15}
	bot := commit.Identity{Name: "", Email: "bot@example.com", When: bob.When, TZOffset: bob.TZOffset}
	// The second vector annotated in architecture/commits.md.
	conflicted := commit.Commit{
		Tree: emptyDir, Parents: []key.Key{keyOf(rootA)}, Author: ann, Committer: bot, Message: "conflict\n",
		ChangeID: changeID, ConflictTerms: []key.Key{removed, added}, ConflictLabels: []string{"ours", "", "theirs"},
	}
	cases = append(cases, named{"conflicted", conflicted})

	withID := rootA
	withID.ChangeID = []byte{0xab}
	cases = append(cases, named{"change id, one byte", withID})

	everything := conflicted
	everything.ChangeID = smData(33, commit.MaxChangeIDLen)
	everything.Signature, everything.PublicKey = smData(31, 64), smData(32, 68)
	cases = append(cases, named{"every key 0-9: signed, 64-byte change id, conflict with labels", everything})

	// No labels, a term that repeats, a DirNode tree.
	cases = append(cases, named{"conflict without labels, a repeated term", commit.Commit{
		Tree: dirNode, Author: ann, Committer: bob, Message: "unlabelled",
		ConflictTerms: []key.Key{removed, removed, added, removed},
	}})

	// 254 terms and 255 labels: both array heads grow a length byte. Only the
	// last label is set; one is 300 bytes of text past the one-byte string
	// length, with code points the character rule lets through (U+0085,
	// U+2028).
	var terms []key.Key
	for i := 0; i < commit.MaxConflictTerms; i++ {
		typ := key.DirLeaf
		if i%2 == 1 {
			typ = key.DirNode
		}
		k, err := fabKey(typ, uint64(i)*1000, byte(i))
		if err != nil {
			return err
		}
		terms = append(terms, k)
	}
	labels := make([]string, 1+len(terms))
	labels[len(labels)-1] = "wqnwkozp 2768b0b9 \"" + strings.Repeat("subject ✓ ", 28) + "\u0085\u2028\" (rebased revision)"
	cases = append(cases, named{"max terms, all labels but the last empty", commit.Commit{
		Tree: emptyDir, Parents: []key.Key{keyOf(rootA), keyOf(rootB)}, Author: ann, Committer: bob, Message: "octopus conflict",
		ChangeID: changeID, ConflictTerms: terms, ConflictLabels: labels,
	}})

	nobody := commit.Identity{When: 0, TZOffset: 0}
	cases = append(cases, named{"identities without name or email", commit.Commit{
		Tree: emptyDir, Author: nobody, Committer: nobody, Message: "anonymous",
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
		if len(n.c.ChangeID) > 0 {
			cc.ChangeIDHex = hex.EncodeToString(n.c.ChangeID)
		}
		for _, t := range n.c.ConflictTerms {
			cc.ConflictTerms = append(cc.ConflictTerms, t.String())
		}
		cc.ConflictLabels = n.c.ConflictLabels
		out.Cases = append(out.Cases, cc)
	}
	return writeJSON(filepath.Join(outDir, "commit.json"), out)
}
