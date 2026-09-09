package main

import (
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"

	"github.com/amber-store/core/amberpack"
	"github.com/amber-store/core/fstree"
	"github.com/amber-store/core/key"
)

type rawRecordCase struct {
	Key          string  `json:"key"`
	Payload      Payload `json:"payload"`
	RecordBlake3 string  `json:"record_blake3"`
	RecordHex    string  `json:"record_hex,omitempty"` // only for payloads <= 256 bytes
}

type rawRecordsFile struct {
	Cases []rawRecordCase `json:"cases"`
}

type compressedRecordCase struct {
	RecordHex string  `json:"record_hex"`
	Key       string  `json:"key"`
	Payload   Payload `json:"payload"`
}

type compressedRecordsFile struct {
	Cases []compressedRecordCase `json:"cases"`
}

// recFlags returns the flag byte of an encoded record.
func recFlags(rec []byte) byte { return rec[33] }

// genAmberpack writes amberpack/records_raw.json, records_compressed.json,
// pack_go.bin (every golden-fstree object, manifest order) and pack_empty.bin.
func genAmberpack(outDir string, tree *goldenTree) error {
	dir := filepath.Join(outDir, "amberpack")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}

	// --- records_raw.json: splitmix payloads never compress, so the encoded
	// record must keep flag 0 and its bytes are implementation-independent.
	type rawIn struct {
		t       key.Type
		length  uint64 // 0 means "use the payload length"
		payload Payload
	}
	rawCases := []rawIn{
		{key.Blob, 0, SM(700, 0)}, // empty payload
		{key.Blob, 0, SM(701, 1)},
		{key.Blob, 0, SM(702, 33)},
		{key.Blob, 0, SM(703, 100)},
		{key.Blob, 0, SM(704, 256)}, // largest payload with record_hex
		{key.Blob, 0, SM(705, 257)}, // smallest payload without record_hex
		{key.Blob, 0, SM(706, 1000)},
		{key.Blob, 0, SM(707, 65536)},
		// A non-Blob key with a logical length differing from the payload
		// length: record framing carries the key verbatim.
		{key.FileNode, 123456789, SM(708, 200)},
	}
	raw := rawRecordsFile{Cases: make([]rawRecordCase, 0, len(rawCases))}
	for _, c := range rawCases {
		data := c.payload.Materialize()
		length := c.length
		if length == 0 {
			length = uint64(len(data))
		}
		k, err := key.New(c.t, length, data)
		if err != nil {
			return err
		}
		rec, err := amberpack.EncodeRecord(k, data)
		if err != nil {
			return err
		}
		if recFlags(rec) != 0 {
			return fmt.Errorf("records_raw: payload %v unexpectedly compressed (flags %#x)", c.payload, recFlags(rec))
		}
		rc := rawRecordCase{Key: k.String(), Payload: c.payload, RecordBlake3: blake3Hex(rec)}
		if len(data) <= 256 {
			rc.RecordHex = hex.EncodeToString(rec)
		}
		raw.Cases = append(raw.Cases, rc)
	}
	if err := writeJSON(filepath.Join(dir, "records_raw.json"), raw); err != nil {
		return err
	}

	// --- records_compressed.json: compressible payloads; the Go-encoded bytes
	// are decode-only vectors for Rust.
	compCases := []Payload{
		Const(0x00, 1000),
		Const(0xAB, 5000),
		Concat(Const(0x11, 400), Const(0x22, 400)),
		Concat(SM(710, 50), Const(0x00, 5000)),
		Const(0x42, 200000),
	}
	comp := compressedRecordsFile{Cases: make([]compressedRecordCase, 0, len(compCases))}
	for _, p := range compCases {
		data := p.Materialize()
		k, err := key.New(key.Blob, uint64(len(data)), data)
		if err != nil {
			return err
		}
		rec, err := amberpack.EncodeRecord(k, data)
		if err != nil {
			return err
		}
		if recFlags(rec)&0x01 == 0 {
			return fmt.Errorf("records_compressed: payload %v did not compress", p)
		}
		comp.Cases = append(comp.Cases, compressedRecordCase{
			RecordHex: hex.EncodeToString(rec),
			Key:       k.String(),
			Payload:   p,
		})
	}
	if err := writeJSON(filepath.Join(dir, "records_compressed.json"), comp); err != nil {
		return err
	}

	// --- pack_go.bin: a wire pack of every golden-fstree object in manifest
	// order.
	packF, err := os.Create(filepath.Join(dir, "pack_go.bin"))
	if err != nil {
		return err
	}
	w := amberpack.NewWriter(packF)
	for _, k := range tree.sorted {
		if err := w.Add(fstree.Object{Key: k, Bytes: tree.objs[k]}); err != nil {
			packF.Close()
			return err
		}
	}
	if err := w.Close(); err != nil {
		packF.Close()
		return err
	}
	if err := packF.Close(); err != nil {
		return err
	}

	// --- pack_empty.bin: magic + end marker only.
	emptyF, err := os.Create(filepath.Join(dir, "pack_empty.bin"))
	if err != nil {
		return err
	}
	we := amberpack.NewWriter(emptyF)
	if err := we.Close(); err != nil {
		emptyF.Close()
		return err
	}
	return emptyF.Close()
}
