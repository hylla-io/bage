package wal

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/hylla-io/bage/internal/locator"
)

// TestReplayTornTrailingRecord verifies that a torn final record (a crash
// mid-Append) does not discard the cleanly-committed records before it.
func TestReplayTornTrailingRecord(t *testing.T) {
	dir := t.TempDir()
	good := Intent{ID: "good", Edits: []locator.FileEdit{{Path: "a.go", StartByte: 0, EndByte: 1, NewText: "x"}}}
	if err := Append(dir, good); err != nil {
		t.Fatalf("Append: %v", err)
	}

	// Simulate a crash mid-write: append a partial, unterminated JSON record.
	f, err := os.OpenFile(filepath.Join(dir, logName), os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		t.Fatalf("open log: %v", err)
	}
	if _, err := f.WriteString(`{"id":"torn`); err != nil {
		t.Fatalf("write torn record: %v", err)
	}
	f.Close()

	got, err := Replay(dir)
	if err != nil {
		t.Fatalf("Replay error = %v, want nil (torn tail must be tolerated)", err)
	}
	if len(got) != 1 || got[0].ID != "good" {
		t.Fatalf("Replay = %#v, want exactly the one good intent", got)
	}
}

// TestReplayMissing verifies that Replay on a non-existent dir or log file
// returns an empty slice and no error rather than failing.
func TestReplayMissing(t *testing.T) {
	tests := []struct {
		name string
		dir  string
	}{
		{name: "missing dir", dir: filepath.Join(t.TempDir(), "nope")},
		{name: "empty dir no log", dir: t.TempDir()},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := Replay(tt.dir)
			if err != nil {
				t.Fatalf("Replay(%q) error = %v, want nil", tt.dir, err)
			}
			if len(got) != 0 {
				t.Fatalf("Replay(%q) = %d intents, want 0", tt.dir, len(got))
			}
		})
	}
}

// TestAppendReplayRoundTrip verifies that appended intents replay identically,
// including Originals bytes and both hash maps, and that multiple intents come
// back in append order. JSON []byte round-tripping is exercised by Originals.
func TestAppendReplayRoundTrip(t *testing.T) {
	tests := []struct {
		name    string
		intents []Intent
	}{
		{
			name: "single intent full fields",
			intents: []Intent{
				{
					ID: "intent-1",
					Edits: []locator.FileEdit{
						{Path: "a.go", StartByte: 0, EndByte: 3, NewText: "foo"},
						{Path: "a.go", StartByte: 10, EndByte: 12, NewText: "x"},
					},
					Originals: map[string][]byte{
						"a.go": {0x00, 0x01, 0xff, 0xfe, '\n', 'h', 'i'},
					},
					ExpectedRawHash:  map[string]string{"a.go": "deadbeef"},
					ExpectedNormHash: map[string]string{"a.go": "cafebabe"},
				},
			},
		},
		{
			name: "multiple intents preserve order",
			intents: []Intent{
				{
					ID:               "first",
					Edits:            []locator.FileEdit{{Path: "one.txt", StartByte: 1, EndByte: 2, NewText: "A"}},
					Originals:        map[string][]byte{"one.txt": {1, 2, 3}},
					ExpectedRawHash:  map[string]string{"one.txt": "11"},
					ExpectedNormHash: map[string]string{"one.txt": "22"},
				},
				{
					ID:               "second",
					Edits:            []locator.FileEdit{{Path: "two.txt", StartByte: 4, EndByte: 5, NewText: "B"}},
					Originals:        map[string][]byte{"two.txt": {4, 5, 6}},
					ExpectedRawHash:  map[string]string{"two.txt": "33"},
					ExpectedNormHash: map[string]string{"two.txt": "44"},
				},
				{
					ID:               "third",
					Edits:            []locator.FileEdit{{Path: "three.txt", StartByte: 7, EndByte: 8, NewText: "C"}},
					Originals:        map[string][]byte{"three.txt": {7, 8, 9}},
					ExpectedRawHash:  map[string]string{"three.txt": "55"},
					ExpectedNormHash: map[string]string{"three.txt": "66"},
				},
			},
		},
		{
			name: "empty maps and nil slices",
			intents: []Intent{
				{ID: "minimal"},
			},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			dir := t.TempDir()
			for _, in := range tt.intents {
				if err := Append(dir, in); err != nil {
					t.Fatalf("Append(%q) error = %v", in.ID, err)
				}
			}

			got, err := Replay(dir)
			if err != nil {
				t.Fatalf("Replay error = %v", err)
			}
			if len(got) != len(tt.intents) {
				t.Fatalf("Replay returned %d intents, want %d", len(got), len(tt.intents))
			}
			for i, want := range tt.intents {
				assertIntentEqual(t, i, got[i], want)
			}
		})
	}
}

// assertIntentEqual compares a replayed intent against the appended original,
// tolerating the nil/empty-map distinction that JSON does not preserve.
func assertIntentEqual(t *testing.T, idx int, got, want Intent) {
	t.Helper()
	if got.ID != want.ID {
		t.Errorf("intent[%d].ID = %q, want %q", idx, got.ID, want.ID)
	}
	if !slicesEqual(got.Edits, want.Edits) {
		t.Errorf("intent[%d].Edits = %#v, want %#v", idx, got.Edits, want.Edits)
	}
	if !bytesMapEqual(got.Originals, want.Originals) {
		t.Errorf("intent[%d].Originals = %#v, want %#v", idx, got.Originals, want.Originals)
	}
	if !strMapEqual(got.ExpectedRawHash, want.ExpectedRawHash) {
		t.Errorf("intent[%d].ExpectedRawHash = %#v, want %#v", idx, got.ExpectedRawHash, want.ExpectedRawHash)
	}
	if !strMapEqual(got.ExpectedNormHash, want.ExpectedNormHash) {
		t.Errorf("intent[%d].ExpectedNormHash = %#v, want %#v", idx, got.ExpectedNormHash, want.ExpectedNormHash)
	}
}

// slicesEqual reports whether two FileEdit slices are equal, treating nil and
// empty as equivalent.
func slicesEqual(a, b []locator.FileEdit) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// bytesMapEqual reports whether two map[string][]byte values are equal,
// treating nil and empty as equivalent.
func bytesMapEqual(a, b map[string][]byte) bool {
	if len(a) != len(b) {
		return false
	}
	for k, av := range a {
		bv, ok := b[k]
		if !ok || !reflect.DeepEqual(av, bv) {
			return false
		}
	}
	return true
}

// strMapEqual reports whether two map[string]string values are equal, treating
// nil and empty as equivalent.
func strMapEqual(a, b map[string]string) bool {
	if len(a) != len(b) {
		return false
	}
	for k, av := range a {
		if bv, ok := b[k]; !ok || av != bv {
			return false
		}
	}
	return true
}
