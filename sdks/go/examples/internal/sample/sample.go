// Package sample generates a pair of synthetic builds (v1 and v2) so
// the CAVS examples can be run end to end without bringing your own data.
//
// v2 is derived from v1 with a realistic mix of changes: some files stay
// identical, one is patched in place, one is brand new, and one is removed.
// The payloads are large and mostly repetitive on purpose — that is exactly
// the shape CAVS is built to exploit, so the update ends up far smaller than
// a full re-download.
package sample

import (
	"fmt"
	"os"
	"path/filepath"
)

// Build describes one generated build directory.
type Build struct {
	Name string
	Dir  string
}

// Generate writes two builds under root and returns their directories.
//
//	root/
//	  Build_v1/
//	  Build_v2/
func Generate(root string) (v1, v2 Build, err error) {
	v1 = Build{Name: "Build_v1", Dir: filepath.Join(root, "Build_v1")}
	v2 = Build{Name: "Build_v2", Dir: filepath.Join(root, "Build_v2")}

	// --- v1: the "shipped" build ---------------------------------------
	files1 := map[string][]byte{
		"app.exe":            filler("engine-core", 512*1024),
		"data/level1.pak":     filler("level-one", 2*1024*1024),
		"data/level2.pak":     filler("level-two", 2*1024*1024),
		"assets/textures.bin": filler("textures", 3*1024*1024),
		"README.txt":          []byte("CAVS demo build v1\n"),
	}
	if err = writeTree(v1.Dir, files1); err != nil {
		return v1, v2, err
	}

	// --- v2: the "patch" build ------------------------------------------
	// level1.pak + textures.bin: byte-for-byte identical (fully reused).
	// app.exe: a small region changed (mostly reused).
	// level2.pak: a new tail appended (mostly reused).
	// level3.pak: brand new.
	// README.txt: deleted.
	files2 := map[string][]byte{
		"app.exe":            patch(files1["app.exe"], 4096, "engine-core v2 hotfix"),
		"data/level1.pak":     files1["data/level1.pak"],
		"data/level2.pak":     append(files1["data/level2.pak"], filler("level-two-dlc", 256*1024)...),
		"data/level3.pak":     filler("level-three", 2*1024*1024),
		"assets/textures.bin": files1["assets/textures.bin"],
	}
	if err = writeTree(v2.Dir, files2); err != nil {
		return v1, v2, err
	}
	return v1, v2, nil
}

// filler returns n bytes of deterministic, compressible-but-not-trivial
// content seeded by tag, so different files differ while staying stable
// across runs.
func filler(tag string, n int) []byte {
	seed := fmt.Appendf(nil, "[%s]-cavs-sample-block-", tag)
	out := make([]byte, n)
	for i := range out {
		out[i] = seed[i%len(seed)]
	}
	return out
}

// patch copies src and overwrites a region at off with marker, modelling a
// small localized change inside an otherwise-unchanged file.
func patch(src []byte, off int, marker string) []byte {
	out := make([]byte, len(src))
	copy(out, src)
	for i, b := range []byte(marker) {
		if off+i < len(out) {
			out[off+i] = b
		}
	}
	return out
}

func writeTree(dir string, files map[string][]byte) error {
	for rel, data := range files {
		full := filepath.Join(dir, rel)
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			return err
		}
		if err := os.WriteFile(full, data, 0o644); err != nil {
			return err
		}
	}
	return nil
}
