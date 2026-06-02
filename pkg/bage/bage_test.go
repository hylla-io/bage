package bage

import (
	"errors"
	"testing"

	"github.com/hylla-io/bage/internal/hashing"
)

// TestOpenRejectsMissingWALDir asserts Open requires a WALDir.
func TestOpenRejectsMissingWALDir(t *testing.T) {
	t.Parallel()
	if _, err := Open(Config{Lang: LangGo}); err == nil {
		t.Fatal("Open with empty WALDir: want error, got nil")
	}
}

// TestOpenAllowsMissingLang asserts Open SUCCEEDS without a Lang: LangUnknown
// (the zero value) selects per-file auto-detection via LangForPath, so an agent
// IDE can open a mixed-language tree. WALDir remains the only hard requirement.
func TestOpenAllowsMissingLang(t *testing.T) {
	t.Parallel()
	ed, err := Open(Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open without Lang (auto-detect mode): unexpected error: %v", err)
	}
	if ed == nil {
		t.Fatal("Open without Lang returned nil Editor")
	}
}

// TestOpenDefaultsHasher asserts a nil Config.Hasher defaults to XXHasher{}.
func TestOpenDefaultsHasher(t *testing.T) {
	t.Parallel()
	ed, err := Open(Config{Lang: LangGo, WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: unexpected error: %v", err)
	}
	if _, ok := ed.hasher.(hashing.XXHasher); !ok {
		t.Fatalf("default hasher: got %T, want hashing.XXHasher", ed.hasher)
	}
}

// TestOpenSucceedsWithValidConfig asserts a fully valid Config opens an Editor
// whose Parser is wired.
func TestOpenSucceedsWithValidConfig(t *testing.T) {
	t.Parallel()
	ed, err := Open(Config{Lang: LangGo, WALDir: t.TempDir(), Hasher: XXHasher{}})
	if err != nil {
		t.Fatalf("Open: unexpected error: %v", err)
	}
	if ed.Parser() == nil {
		t.Fatal("Parser() returned nil")
	}
	if err := ed.Close(); err != nil {
		t.Fatalf("Close: unexpected error: %v", err)
	}
}

// TestRenameRequiresLSPCommand asserts Rename errors when no LSPCommand is set,
// without spawning any server (hermetic).
func TestRenameRequiresLSPCommand(t *testing.T) {
	t.Parallel()
	ed, err := Open(Config{Lang: LangGo, WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: unexpected error: %v", err)
	}
	if _, err := ed.Rename(t.Context(), "x.go", 0, 0, "y"); err == nil {
		t.Fatal("Rename without LSPCommand: want error, got nil")
	}
}

// TestErrConflictAliasIdentity asserts the re-exported ErrConflict is the same
// sentinel session callers match with errors.Is via a ConflictError.
func TestErrConflictAliasIdentity(t *testing.T) {
	t.Parallel()
	var ce *ConflictError = &ConflictError{Path: "f.go", Reason: "conflict"}
	if !errors.Is(ce, ErrConflict) {
		t.Fatal("ConflictError does not match the re-exported ErrConflict")
	}
}
