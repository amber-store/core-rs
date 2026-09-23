package main

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/amber-store/core/key"
	"github.com/amber-store/core/packstore"
)

const segmentSize = 65536

type segObject struct {
	Key     string  `json:"key"`
	Payload Payload `json:"payload"`
}

type segmentsManifest struct {
	SegmentSize int         `json:"segment_size"`
	Objects     []segObject `json:"objects"`
	Absent      []string    `json:"absent"`
}

// genSegments writes segments_go/: a real packstore directory with two sealed
// segments and one unsealed active segment, plus its manifest.
//
// Method for the unsealed tail: packstore.Store.Close fsyncs and closes the
// active segment WITHOUT sealing it (sealing only happens when an append
// pushes the segment past the size threshold), so no crash simulation is
// needed. We Put objects sequentially (deterministic order, hence
// deterministic bytes) until the directory holds two sealed *.seg files, then
// Put three small tail objects — which start the third segment and stay far
// below the threshold — and Close. The result is exactly the "killed before
// seal" on-disk state: an active segment with valid records and no footer.
func genSegments(outDir string) error {
	dir := filepath.Join(outDir, "segments_go")
	st, err := packstore.Open(dir, packstore.WithSegmentSize(segmentSize), packstore.WithSync(false))
	if err != nil {
		return err
	}
	defer st.Close()

	sealedCount := func() (int, error) {
		ents, err := os.ReadDir(dir)
		if err != nil {
			return 0, err
		}
		n := 0
		for _, e := range ents {
			if strings.HasSuffix(e.Name(), ".seg") {
				n++
			}
		}
		return n, nil
	}

	man := segmentsManifest{SegmentSize: segmentSize}
	put := func(p Payload) error {
		data := p.Materialize()
		k, err := key.New(key.Blob, uint64(len(data)), data)
		if err != nil {
			return err
		}
		if err := st.Put(k, data); err != nil {
			return err
		}
		man.Objects = append(man.Objects, segObject{Key: k.String(), Payload: p})
		return nil
	}

	// Fill until two segments have been sealed. Mix incompressible splitmix
	// payloads with compressible const payloads so sealed bodies contain both
	// raw and zstd records.
	for i := 0; ; i++ {
		if i >= 200 {
			return fmt.Errorf("segments_go: 200 objects written without reaching two seals")
		}
		n, err := sealedCount()
		if err != nil {
			return err
		}
		if n == 2 {
			break
		}
		if n > 2 {
			return fmt.Errorf("segments_go: overshot to %d sealed segments", n)
		}
		var p Payload
		if i%7 == 3 {
			p = Const(byte(0x40+i%32), 20000+i*13)
		} else {
			p = SM(uint64(3000+i), 2500+(i*997)%6000)
		}
		if err := put(p); err != nil {
			return err
		}
	}

	// Tail records: these open the third (active) segment and stay far below
	// the rotation threshold, so it is never sealed.
	for i := 0; i < 3; i++ {
		if err := put(SM(uint64(4000+i), 1000+i*200)); err != nil {
			return err
		}
	}

	// Canonical keys that must report not-found.
	absent := []Payload{SM(9990, 11), SM(9991, 22), SM(9992, 33), SM(9993, 44)}
	for _, p := range absent {
		data := p.Materialize()
		k, err := key.New(key.Blob, uint64(len(data)), data)
		if err != nil {
			return err
		}
		has, err := st.Has(k)
		if err != nil {
			return err
		}
		if has {
			return fmt.Errorf("segments_go: absent key %s is unexpectedly stored", k)
		}
		man.Absent = append(man.Absent, k.String())
	}

	if err := st.Close(); err != nil {
		return err
	}

	// Post-conditions: exactly two sealed segments and one active segment with
	// records but no footer.
	ents, err := os.ReadDir(dir)
	if err != nil {
		return err
	}
	var sealed, active []string
	for _, e := range ents {
		switch {
		case strings.HasSuffix(e.Name(), ".seg.active"):
			active = append(active, e.Name())
		case strings.HasSuffix(e.Name(), ".seg"):
			sealed = append(sealed, e.Name())
		}
	}
	if len(sealed) != 2 || len(active) != 1 {
		return fmt.Errorf("segments_go: got %d sealed + %d active segments, want 2 + 1", len(sealed), len(active))
	}
	ab, err := os.ReadFile(filepath.Join(dir, active[0]))
	if err != nil {
		return err
	}
	if len(ab) <= 8 {
		return fmt.Errorf("segments_go: active segment has no records (%d bytes)", len(ab))
	}
	if bytes.HasSuffix(ab, []byte("AMBERSGF")) {
		return fmt.Errorf("segments_go: active segment unexpectedly carries a footer")
	}
	// The active segment's sidecar index (architecture/packstore.md) is part
	// of the fixture: magic, one entry per tail record, and the synced record
	// that Close writes. The sealed segments' sidecars went with their seals.
	// gc.lock, which every open creates, holds nothing a reader needs.
	idx, err := os.ReadFile(filepath.Join(dir, active[0]+".idx"))
	if err != nil {
		return fmt.Errorf("segments_go: the active segment has no sidecar: %w", err)
	}
	if want := 8 + 56*(3+1); len(idx) != want {
		return fmt.Errorf("segments_go: sidecar is %d bytes, want %d", len(idx), want)
	}
	if err := os.Remove(filepath.Join(dir, "gc.lock")); err != nil {
		return err
	}

	return writeJSON(filepath.Join(dir, "manifest.json"), man)
}
