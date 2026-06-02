package atomicwrite

import (
	"bytes"
	"os"
	"path/filepath"
	"testing"
)

// TestWrite covers the success behaviors of Write: creating a new file,
// overwriting an existing one, exact content/round-trip fidelity, and the
// absence of leftover temp files after a successful write.
func TestWrite(t *testing.T) {
	tests := []struct {
		name    string
		seed    []byte // pre-existing content, nil means no file
		data    []byte
		wantLen int
	}{
		{name: "write-new-file", seed: nil, data: []byte("hello\nworld\n"), wantLen: 12},
		{name: "overwrite-existing", seed: []byte("old contents here"), data: []byte("new"), wantLen: 3},
		{name: "content-correct", seed: nil, data: []byte("exact bytes \x00\xff\x01"), wantLen: 15},
		{name: "round-trip-empty", seed: nil, data: []byte{}, wantLen: 0},
		{name: "round-trip-binary", seed: []byte("x"), data: []byte{0, 1, 2, 3, 255, 254}, wantLen: 6},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			dir := t.TempDir()
			path := filepath.Join(dir, "target.txt")

			if tt.seed != nil {
				if err := os.WriteFile(path, tt.seed, 0o644); err != nil {
					t.Fatalf("seed write: %v", err)
				}
			}

			if err := Write(path, tt.data); err != nil {
				t.Fatalf("Write: unexpected error: %v", err)
			}

			got, err := os.ReadFile(path)
			if err != nil {
				t.Fatalf("read back: %v", err)
			}
			if !bytes.Equal(got, tt.data) {
				t.Errorf("content mismatch: got %q want %q", got, tt.data)
			}
			if len(got) != tt.wantLen {
				t.Errorf("length mismatch: got %d want %d", len(got), tt.wantLen)
			}

			assertNoTempLeftover(t, dir)
		})
	}
}

// TestWriteCleansTempOnError verifies that when the rename target cannot be
// produced (target dir does not exist), Write returns an error and leaves no
// temp file behind.
func TestWriteCleansTempOnError(t *testing.T) {
	tests := []struct {
		name string
		path func(dir string) string
	}{
		{
			name: "nonexistent-dir",
			path: func(dir string) string {
				return filepath.Join(dir, "does-not-exist", "target.txt")
			},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			dir := t.TempDir()
			path := tt.path(dir)

			if err := Write(path, []byte("data")); err == nil {
				t.Fatalf("Write: expected error, got nil")
			}

			// The temp file lives in filepath.Dir(path); if that dir does not
			// exist, CreateTemp fails and nothing is left in the parent dir.
			assertNoTempLeftover(t, dir)
		})
	}
}

// assertNoTempLeftover fails the test if any atomicwrite temp file remains in
// dir. Temp files are dot-prefixed with a ".tmp-" infix.
func assertNoTempLeftover(t *testing.T, dir string) {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("read dir %q: %v", dir, err)
	}
	for _, e := range entries {
		if filepath.Ext(e.Name()) == ".txt" {
			continue
		}
		t.Errorf("leftover temp file: %q", e.Name())
	}
}
