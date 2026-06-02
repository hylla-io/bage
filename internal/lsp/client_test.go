package lsp

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"go.lsp.dev/uri"
)

// TestClientRenameSmoke exercises the real client wiring against gopls. It is
// hermetic-by-skip: if gopls is not installed the test is skipped so the suite
// never depends on a live server. The pure conversion logic is covered without a
// server in convert_test.go.
func TestClientRenameSmoke(t *testing.T) {
	if _, err := exec.LookPath("gopls"); err != nil {
		t.Skip("gopls not on PATH; skipping live LSP smoke test")
	}

	root := t.TempDir()
	writeFile(t, filepath.Join(root, "go.mod"), "module smoke\n\ngo 1.21\n")
	srcPath := filepath.Join(root, "main.go")
	src := "package main\n\nfunc greet() string { return \"hi\" }\n\nfunc main() { _ = greet() }\n"
	writeFile(t, srcPath, src)

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	c, err := NewClient(ctx, []string{"gopls"})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	defer func() {
		closeCtx, closeCancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer closeCancel()
		_ = c.Close(closeCtx)
	}()

	if err := c.Initialize(ctx, uri.File(root)); err != nil {
		t.Fatalf("Initialize: %v", err)
	}

	// "func greet" — greet starts at character 5 on line 2 (zero-based).
	we, err := c.Rename(ctx, srcPath, src, 2, 5, "salute")
	if err != nil {
		t.Fatalf("Rename: %v", err)
	}

	edits, err := WorkspaceEditToFileEdits(we, os.ReadFile)
	if err != nil {
		t.Fatalf("WorkspaceEditToFileEdits: %v", err)
	}
	if len(edits) == 0 {
		t.Fatalf("expected at least one rename edit, got none")
	}
	for _, e := range edits {
		if e.NewText != "salute" {
			t.Fatalf("unexpected edit NewText %q in %+v", e.NewText, e)
		}
	}
}

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatalf("write %q: %v", path, err)
	}
}
