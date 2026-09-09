package main

import (
	"path/filepath"
	"strconv"

	"github.com/amber-store/core/key"
)

type keyCase struct {
	Type    int     `json:"type"`
	Length  string  `json:"length"` // decimal, may exceed 2^53
	Payload Payload `json:"payload"`
	Key     string  `json:"key"`
}

type keysFile struct {
	Cases []keyCase `json:"cases"`
}

// genKeys writes keys.json: key.New over all five types, every length-field
// size 1..8, the zero-length special case, and logical lengths that differ
// from the payload length.
func genKeys(outDir string) error {
	type in struct {
		t       key.Type
		length  uint64
		payload Payload
	}
	cases := []in{
		// Length 0 special case (length-field size 1, single 0x00 byte).
		{key.Blob, 0, SM(0, 0)},
		// Length-field size 1.
		{key.Blob, 1, SM(100, 1)},
		{key.FileNode, 255, SM(101, 255)},
		// Length-field size 2.
		{key.DirLeaf, 256, SM(102, 256)},
		{key.DirNode, 65535, SM(103, 65535)},
		// Length-field size 3.
		{key.XattrSet, 65536, SM(104, 65536)},
		// Length-field sizes 4..8: logical lengths (payload is small).
		{key.Blob, 1 << 24, SM(105, 64)},
		{key.FileNode, 1 << 32, SM(106, 64)},
		{key.DirLeaf, 1 << 40, SM(107, 64)},
		{key.DirNode, 1 << 48, SM(108, 64)},
		{key.XattrSet, 1<<53 + 1, SM(109, 64)},
		{key.Blob, 1<<64 - 1, SM(110, 64)},
		// Logical length smaller than the payload length.
		{key.Blob, 5, SM(111, 100)},
		// Const and concat payload derivations.
		{key.FileNode, 1000000, Const(0x7F, 10)},
		{key.DirLeaf, 300, Concat(SM(112, 100), Const(0x01, 200))},
	}

	out := keysFile{Cases: make([]keyCase, 0, len(cases))}
	for _, c := range cases {
		k, err := key.New(c.t, c.length, c.payload.Materialize())
		if err != nil {
			return err
		}
		out.Cases = append(out.Cases, keyCase{
			Type:    int(c.t),
			Length:  strconv.FormatUint(c.length, 10),
			Payload: c.payload,
			Key:     k.String(),
		})
	}
	return writeJSON(filepath.Join(outDir, "keys.json"), out)
}
