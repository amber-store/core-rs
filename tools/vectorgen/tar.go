package main

import (
	"os"
	"path/filepath"

	"github.com/amber-store/core/tarexport"
)

// genTar writes tar_go.tar: tarexport.Write of the golden tree, served by an
// in-memory Getter over the generated objects.
func genTar(outDir string, tree *goldenTree) error {
	f, err := os.Create(filepath.Join(outDir, "tar_go.tar"))
	if err != nil {
		return err
	}
	if err := tarexport.Write(f, tree.root, tree.get); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}
