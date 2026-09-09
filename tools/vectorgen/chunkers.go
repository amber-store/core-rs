package main

import (
	"bytes"
	"fmt"
	"path/filepath"

	"github.com/amber-store/core/chunkers"
)

type ultracdcCase struct {
	Name   string  `json:"name"`
	Min    int     `json:"min"`
	Normal int     `json:"normal"`
	Max    int     `json:"max"`
	Data   Payload `json:"data"`
	Chunks []int   `json:"chunks"`
}

type ultracdcFile struct {
	Cases []ultracdcCase `json:"cases"`
}

// genUltraCDC writes ultracdc.json: chunk-length sequences produced by
// chunkers.SplitBytes. min/normal/max of 0 select the library defaults
// (chunkers.DefaultMinSize/DefaultNormalSize/DefaultMaxSize).
func genUltraCDC(outDir string) error {
	type in struct {
		name             string
		min, normal, max int
		data             Payload
	}
	cases := []in{
		{"empty", 0, 0, 0, SM(200, 0)},
		{"one-byte", 0, 0, 0, SM(201, 1)},
		{"min-exact", 0, 0, 0, SM(202, chunkers.DefaultMinSize)},
		{"min-plus-1", 0, 0, 0, SM(203, chunkers.DefaultMinSize+1)},
		{"max-exact", 0, 0, 0, SM(204, chunkers.DefaultMaxSize)},
		{"big-splitmix", 0, 0, 0, SM(205, 16*1024*1024+12345)},
		{"const-lest-aa", 0, 0, 0, Const(0xAA, 500000)},
		{"const-zero", 0, 0, 0, Const(0x00, 300000)},
		{"mixed-concat", 0, 0, 0, Concat(SM(206, 100000), Const(0x55, 200000), SM(207, 150000))},
		{"custom-small", 64, 128, 256, SM(208, 10000)},
		{"custom-mixed", 512, 1024, 4096, Concat(Const(0xFF, 30000), SM(209, 20000))},
	}

	out := ultracdcFile{Cases: make([]ultracdcCase, 0, len(cases))}
	for _, c := range cases {
		var opts *chunkers.ByteOpts
		if c.min != 0 || c.normal != 0 || c.max != 0 {
			opts = &chunkers.ByteOpts{MinSize: c.min, NormalSize: c.normal, MaxSize: c.max}
		}
		data := c.data.Materialize()
		chunkLens := make([]int, 0)
		sum := 0
		err := chunkers.SplitBytes(bytes.NewReader(data), opts, func(chunk []byte) error {
			chunkLens = append(chunkLens, len(chunk))
			sum += len(chunk)
			return nil
		})
		if err != nil {
			return fmt.Errorf("%s: %w", c.name, err)
		}
		if sum != len(data) {
			return fmt.Errorf("%s: chunk lengths sum to %d, input is %d", c.name, sum, len(data))
		}
		out.Cases = append(out.Cases, ultracdcCase{
			Name: c.name, Min: c.min, Normal: c.normal, Max: c.max,
			Data: c.data, Chunks: chunkLens,
		})
	}
	return writeJSON(filepath.Join(outDir, "ultracdc.json"), out)
}

type itemChunkerCase struct {
	Bits  int       `json:"bits"`
	Items []Payload `json:"items"`
	Runs  []int     `json:"runs"`
}

type itemChunkerFile struct {
	Cases []itemChunkerCase `json:"cases"`
}

// genItemChunker writes item_chunker.json: each item's encoding is fed to
// ItemChunker.IsBoundary(enc, runLen) in order, where runLen counts the items
// of the current run including the current one; a true result closes the run.
// The final (possibly unterminated) run is included.
func genItemChunker(outDir string) error {
	type in struct {
		bits     int
		count    int
		seedBase uint64
		lenMod   int
	}
	cases := []in{
		{bits: 0, count: 11, seedBase: 300, lenMod: 5},
		{bits: 4, count: 200, seedBase: 400, lenMod: 23},
		{bits: 7, count: 1200, seedBase: 500, lenMod: 17},
		{bits: 10, count: 3000, seedBase: 600, lenMod: 13},
	}

	out := itemChunkerFile{Cases: make([]itemChunkerCase, 0, len(cases))}
	for _, c := range cases {
		ic := chunkers.NewItemChunker(c.bits)
		items := make([]Payload, 0, c.count)
		runs := make([]int, 0)
		runLen := 0
		for i := 0; i < c.count; i++ {
			p := SM(c.seedBase+uint64(i), 8+i%c.lenMod)
			items = append(items, p)
			runLen++
			if ic.IsBoundary(p.Materialize(), runLen) {
				runs = append(runs, runLen)
				runLen = 0
			}
		}
		if runLen > 0 {
			runs = append(runs, runLen)
		}
		out.Cases = append(out.Cases, itemChunkerCase{Bits: c.bits, Items: items, Runs: runs})
	}
	return writeJSON(filepath.Join(outDir, "item_chunker.json"), out)
}
