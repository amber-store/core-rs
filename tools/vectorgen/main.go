// Command vectorgen generates the golden test vectors described in
// core-rs/VECTORS.md by driving the Go implementation of
// github.com/amber-store/core (the normative reference for the
// Rust port). Usage:
//
//	go run . ../../tests/golden
//
// Every output is deterministic: running twice produces identical bytes.
package main

import (
	"fmt"
	"os"
	"path/filepath"
)

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: vectorgen <output-dir>")
		os.Exit(2)
	}
	outDir := os.Args[1]
	if err := run(outDir); err != nil {
		fmt.Fprintln(os.Stderr, "vectorgen:", err)
		os.Exit(1)
	}
}

// run wipes the generator-owned outputs and regenerates all of them.
func run(outDir string) error {
	if err := os.MkdirAll(outDir, 0o755); err != nil {
		return err
	}
	// Remove exactly the files/dirs this generator owns, so regeneration is a
	// clean rebuild (stale packstore segments would otherwise be resumed).
	owned := []string{
		"keys.json", "ultracdc.json", "item_chunker.json", "filters.json",
		"reference.json", "amberignore.json", "tar_go.tar",
		"fstree", "amberpack", "segments_go",
	}
	for _, name := range owned {
		if err := os.RemoveAll(filepath.Join(outDir, name)); err != nil {
			return err
		}
	}

	if err := genKeys(outDir); err != nil {
		return fmt.Errorf("keys.json: %w", err)
	}
	if err := genUltraCDC(outDir); err != nil {
		return fmt.Errorf("ultracdc.json: %w", err)
	}
	if err := genItemChunker(outDir); err != nil {
		return fmt.Errorf("item_chunker.json: %w", err)
	}
	tree, err := genFstree(outDir)
	if err != nil {
		return fmt.Errorf("fstree: %w", err)
	}
	if err := genAmberpack(outDir, tree); err != nil {
		return fmt.Errorf("amberpack: %w", err)
	}
	if err := genFilters(outDir); err != nil {
		return fmt.Errorf("filters.json: %w", err)
	}
	if err := genSegments(outDir); err != nil {
		return fmt.Errorf("segments_go: %w", err)
	}
	if err := genTar(outDir, tree); err != nil {
		return fmt.Errorf("tar_go.tar: %w", err)
	}
	if err := genAmberignore(outDir); err != nil {
		return fmt.Errorf("amberignore.json: %w", err)
	}
	if err := genReference(outDir); err != nil {
		return fmt.Errorf("reference.json: %w", err)
	}
	return nil
}
