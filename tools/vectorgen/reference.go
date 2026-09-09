package main

import (
	"bytes"
	"encoding/hex"
	"fmt"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/amber-store/core/key"
	"github.com/amber-store/core/reference"
)

type referenceCase struct {
	Name         string `json:"name"`
	Key          string `json:"key"`
	User         string `json:"user"`
	CreatedAt    string `json:"created_at"` // decimal int64, ns
	SignatureHex string `json:"signature_hex,omitempty"`
	PublicKeyHex string `json:"public_key_hex,omitempty"`
	BytesHex     string `json:"bytes_hex"`
}

type referenceFile struct {
	Cases []referenceCase `json:"cases"`
}

// genReference writes reference.json: canonical reference-record encodings.
func genReference(outDir string) error {
	mkKey := func(t key.Type, length uint64, seed uint64, n int) key.Key {
		k, err := key.New(t, length, smData(seed, n))
		if err != nil {
			panic(err)
		}
		return k
	}

	refs := []reference.Reference{
		{ // minimal: empty user, no signature
			Name:      "r",
			Key:       keyBytes(mkKey(key.DirNode, 4096, 20, 64)),
			User:      "",
			CreatedAt: 1700000000000000000,
		},
		{ // unicode name with '/'
			Name:      "refs/π/名前/✓",
			Key:       keyBytes(mkKey(key.Blob, 0, 26, 0)),
			User:      "alice@example.com",
			CreatedAt: 1700000000000000001,
		},
		{ // max-length name (1024 bytes)
			Name:      strings.Repeat("n", 1024),
			Key:       keyBytes(mkKey(key.XattrSet, 300, 23, 300)),
			User:      "bob",
			CreatedAt: 0,
		},
		{ // signed shape: opaque dummy signature + public key
			Name:      "signed/ref",
			Key:       keyBytes(mkKey(key.FileNode, 999999, 24, 10)),
			User:      "carol@example.com",
			CreatedAt: 1712345678901234567,
			Signature: smData(21, 64),
			PublicKey: smData(22, 68),
		},
		{ // negative created_at
			Name:      "old-ref",
			Key:       keyBytes(mkKey(key.DirLeaf, 77, 25, 77)),
			User:      "dave",
			CreatedAt: -1,
		},
	}

	out := referenceFile{Cases: make([]referenceCase, 0, len(refs))}
	for _, r := range refs {
		enc, err := r.Encode()
		if err != nil {
			return fmt.Errorf("%q: %w", r.Name, err)
		}
		dec, err := reference.Decode(enc)
		if err != nil {
			return fmt.Errorf("%q: decode round-trip: %w", r.Name, err)
		}
		reEnc, err := dec.Encode()
		if err != nil {
			return fmt.Errorf("%q: re-encode: %w", r.Name, err)
		}
		if !bytes.Equal(reEnc, enc) {
			return fmt.Errorf("%q: round-trip bytes differ", r.Name)
		}
		rc := referenceCase{
			Name:      r.Name,
			Key:       hex.EncodeToString(r.Key),
			User:      r.User,
			CreatedAt: strconv.FormatInt(r.CreatedAt, 10),
			BytesHex:  hex.EncodeToString(enc),
		}
		if len(r.Signature) > 0 {
			rc.SignatureHex = hex.EncodeToString(r.Signature)
		}
		if len(r.PublicKey) > 0 {
			rc.PublicKeyHex = hex.EncodeToString(r.PublicKey)
		}
		out.Cases = append(out.Cases, rc)
	}
	return writeJSON(filepath.Join(outDir, "reference.json"), out)
}

// keyBytes returns a key's bytes as a fresh slice.
func keyBytes(k key.Key) []byte {
	return append([]byte(nil), k[:]...)
}
