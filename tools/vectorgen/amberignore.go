package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/amber-store/core/amberignore"
)

type ignoreFileSpec struct {
	Dir   string   `json:"dir"` // directory containing the .amberignore, "" = root
	Lines []string `json:"lines"`
}

type ignoreCheck struct {
	Path    string `json:"path"`
	IsDir   bool   `json:"is_dir"`
	Ignored bool   `json:"ignored"`
}

type ignoreCase struct {
	Name   string           `json:"name"`
	Files  []ignoreFileSpec `json:"files"`
	Checks []ignoreCheck    `json:"checks"`
}

type ignoreFile struct {
	Cases []ignoreCase `json:"cases"`
}

// checkSpec is an input check: path + is_dir; the Go matcher decides ignored.
type checkSpec struct {
	path  string
	isDir bool
}

// oracle evaluates one path against a materialized ignore tree exactly as the
// ingest walk does: descend from the root, pruning at the first ignored
// ancestor directory; otherwise the final component's Ignored answer counts.
func oracle(root, path string, isDir bool) (bool, error) {
	m, err := amberignore.Root(root)
	if err != nil {
		return false, err
	}
	comps := strings.Split(path, "/")
	dir := root
	for i, c := range comps {
		if i == len(comps)-1 {
			return m.Ignored(c, isDir), nil
		}
		if m.Ignored(c, true) {
			return true, nil // an ignored directory prunes its whole subtree
		}
		dir = filepath.Join(dir, c)
		if m, err = m.Descend(dir, c); err != nil {
			return false, err
		}
	}
	return false, fmt.Errorf("empty path")
}

// genAmberignore writes amberignore.json, using the Go amberignore package as
// the oracle over .amberignore trees materialized in a temp directory.
func genAmberignore(outDir string) error {
	type in struct {
		name   string
		files  []ignoreFileSpec
		checks []checkSpec
	}
	cases := []in{
		{
			name:  "literal",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"foo.txt"}}},
			checks: []checkSpec{
				{"foo.txt", false},
				{"foo.txt", true},
				{"bar.txt", false},
				{"sub/foo.txt", false},
				{"foo.txt.bak", false},
			},
		},
		{
			name:  "wildcard",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"*.log"}}},
			checks: []checkSpec{
				{"a.log", false},
				{"a.log", true},
				{"b.txt", false},
				{"sub/deep/c.log", false},
				{"log", false},
			},
		},
		{
			name:  "question-and-class",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"file?.txt", "[abc].dat"}}},
			checks: []checkSpec{
				{"file1.txt", false},
				{"file12.txt", false},
				{"a.dat", false},
				{"d.dat", false},
			},
		},
		{
			name:  "dir-only",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"build/"}}},
			checks: []checkSpec{
				{"build", true},
				{"build", false},
				{"build/output.txt", false}, // pruned under the ignored dir
				{"src/build", true},
				{"src/build", false},
			},
		},
		{
			name:  "anchored",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"/top.txt"}}},
			checks: []checkSpec{
				{"top.txt", false},
				{"sub/top.txt", false},
			},
		},
		{
			name:  "anchored-subpath",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"docs/internal"}}},
			checks: []checkSpec{
				{"docs/internal", true},
				{"docs/internal", false},
				{"other/docs/internal", true},
				{"internal", true},
			},
		},
		{
			name:  "doublestar-leading",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"**/temp"}}},
			checks: []checkSpec{
				{"temp", false},
				{"a/temp", false},
				{"a/b/c/temp", false},
				{"a/temperature", false},
			},
		},
		{
			name:  "doublestar-middle",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"a/**/b"}}},
			checks: []checkSpec{
				{"a/b", false},
				{"a/x/b", false},
				{"a/x/y/b", false},
				{"x/a/b", false},
			},
		},
		{
			name:  "doublestar-trailing",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"logs/**"}}},
			checks: []checkSpec{
				{"logs", true},
				{"logs/a.txt", false},
				{"logs/deep/b.txt", false},
				{"other/a.txt", false},
			},
		},
		{
			name:  "negation-last-wins",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"*.log", "!keep.log"}}},
			checks: []checkSpec{
				{"a.log", false},
				{"keep.log", false},
				{"sub/keep.log", false},
				{"sub/other.log", false},
			},
		},
		{
			name:  "negation-then-ignore",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"!keep.log", "*.log"}}},
			checks: []checkSpec{
				{"keep.log", false}, // later *.log wins over earlier negation
				{"other.log", false},
			},
		},
		{
			name: "nested-composition",
			files: []ignoreFileSpec{
				{Dir: "", Lines: []string{"*.tmp"}},
				{Dir: "sub", Lines: []string{"!important.tmp", "local.txt"}},
			},
			checks: []checkSpec{
				{"a.tmp", false},
				{"local.txt", false}, // sub's patterns are scoped to sub
				{"sub/important.tmp", false},
				{"sub/other.tmp", false},
				{"sub/local.txt", false},
				{"sub/deep/important.tmp", false},
				{"sub/deep/local.txt", false},
			},
		},
		{
			name: "nested-anchored",
			files: []ignoreFileSpec{
				{Dir: "sub", Lines: []string{"/only-here.txt"}},
			},
			checks: []checkSpec{
				{"only-here.txt", false},
				{"sub/only-here.txt", false},
				{"sub/nested/only-here.txt", false},
			},
		},
		{
			name: "no-reinclude-under-ignored-dir",
			files: []ignoreFileSpec{
				{Dir: "", Lines: []string{"node_modules/", "!node_modules/keep.js"}},
			},
			checks: []checkSpec{
				{"node_modules", true},
				{"node_modules/keep.js", false}, // pruned: parent dir ignored
				{"node_modules/drop.js", false},
			},
		},
		{
			name:  "comments-and-blanks",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"# a comment", "", "   ", "real.txt"}}},
			checks: []checkSpec{
				{"real.txt", false},
				{"# a comment", false},
				{"a comment", false},
			},
		},
		{
			name:  "ignore-everything-but-amberignore",
			files: []ignoreFileSpec{{Dir: "", Lines: []string{"*"}}},
			checks: []checkSpec{
				{"anything.txt", false},
				{"somedir", true},
				{".amberignore", false}, // the ignore file itself is never ignored
				{".amberignore", true},  // ... but a directory of that name is
			},
		},
	}

	out := ignoreFile{Cases: make([]ignoreCase, 0, len(cases))}
	for _, c := range cases {
		root, err := os.MkdirTemp("", "vectorgen-ignore-")
		if err != nil {
			return err
		}
		for _, f := range c.files {
			dir := filepath.Join(root, filepath.FromSlash(f.Dir))
			if err := os.MkdirAll(dir, 0o755); err != nil {
				os.RemoveAll(root)
				return err
			}
			content := strings.Join(f.Lines, "\n") + "\n"
			if err := os.WriteFile(filepath.Join(dir, amberignore.FileName), []byte(content), 0o644); err != nil {
				os.RemoveAll(root)
				return err
			}
		}
		oc := ignoreCase{Name: c.name, Files: c.files, Checks: make([]ignoreCheck, 0, len(c.checks))}
		for _, ch := range c.checks {
			ignored, err := oracle(root, ch.path, ch.isDir)
			if err != nil {
				os.RemoveAll(root)
				return fmt.Errorf("%s: %s: %w", c.name, ch.path, err)
			}
			oc.Checks = append(oc.Checks, ignoreCheck{Path: ch.path, IsDir: ch.isDir, Ignored: ignored})
		}
		os.RemoveAll(root)
		out.Cases = append(out.Cases, oc)
	}
	return writeJSON(filepath.Join(outDir, "amberignore.json"), out)
}
