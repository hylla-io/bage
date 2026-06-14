package session

import (
	"bytes"
	"encoding/json"
	"testing"

	"github.com/hylla-io/bage/internal/region"
)

// TestDeleteResultRenderText asserts DeleteResult.RenderText writes the exact
// confirmation line the delete CLI prints today: "deleted <path> raw=<h>\n".
func TestDeleteResultRenderText(t *testing.T) {
	r := DeleteResult{Path: "doomed.go", RawHash: "00000000000000aa"}
	var buf bytes.Buffer
	if err := r.RenderText(&buf); err != nil {
		t.Fatalf("RenderText: %v", err)
	}
	want := "deleted doomed.go raw=00000000000000aa\n"
	if buf.String() != want {
		t.Fatalf("RenderText = %q, want %q", buf.String(), want)
	}
}

// TestDeleteResultMarshalsSnakeCaseJSON asserts DeleteResult marshals to flat
// snake_case JSON keys.
func TestDeleteResultMarshalsSnakeCaseJSON(t *testing.T) {
	r := DeleteResult{Path: "doomed.go", RawHash: "00000000000000aa"}
	b, err := json.Marshal(r)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(b, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	want := map[string]any{"path": "doomed.go", "raw_hash": "00000000000000aa"}
	if len(got) != len(want) {
		t.Fatalf("key set = %v, want %v", got, want)
	}
	for k, wv := range want {
		if got[k] != wv {
			t.Fatalf("key %q = %v, want %v", k, got[k], wv)
		}
	}
}

// TestMoveResultRenderText asserts MoveResult.RenderText writes the exact
// confirmation line the move CLI prints today:
// "moved <from> -> <dest> raw=<h>\n", where dest and the hash come from the
// destination EditResult.
func TestMoveResultRenderText(t *testing.T) {
	r := MoveResult{
		From: "old.go",
		Dest: region.EditResult{Path: "new.go", NewFileRawHash: "00000000000000bb"},
	}
	var buf bytes.Buffer
	if err := r.RenderText(&buf); err != nil {
		t.Fatalf("RenderText: %v", err)
	}
	want := "moved old.go -> new.go raw=00000000000000bb\n"
	if buf.String() != want {
		t.Fatalf("RenderText = %q, want %q", buf.String(), want)
	}
}

// TestMoveResultMarshalsSnakeCaseJSON asserts MoveResult marshals to flat
// snake_case top-level keys with the destination EditResult nested under
// "dest".
func TestMoveResultMarshalsSnakeCaseJSON(t *testing.T) {
	r := MoveResult{
		From: "old.go",
		Dest: region.EditResult{Path: "new.go", NewFileRawHash: "00000000000000bb"},
	}
	b, err := json.Marshal(r)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(b, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if got["from"] != "old.go" {
		t.Fatalf("from = %v, want old.go", got["from"])
	}
	dest, ok := got["dest"].(map[string]any)
	if !ok {
		t.Fatalf("dest not an object: %v", got["dest"])
	}
	if dest["path"] != "new.go" {
		t.Fatalf("dest.path = %v, want new.go", dest["path"])
	}
	if dest["new_file_raw_hash"] != "00000000000000bb" {
		t.Fatalf("dest.new_file_raw_hash = %v, want 00000000000000bb", dest["new_file_raw_hash"])
	}
	if len(got) != 2 {
		t.Fatalf("top-level key set = %v, want {from, dest}", got)
	}
}
