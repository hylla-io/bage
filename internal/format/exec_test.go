package format

import (
	"bytes"
	"context"
	"testing"
	"time"
)

// TestCmdFormatter exercises the configured-command formatter over an
// identity round-trip via "cat", a launch failure via a nonexistent command,
// and context cancellation. These cases are hermetic: they rely only on
// universally available POSIX utilities.
func TestCmdFormatter(t *testing.T) {
	t.Run("cat identity round-trip", func(t *testing.T) {
		f := CmdFormatter{Name: "cat"}
		src := []byte("package main\n\nfunc main() {}\n")

		got, err := f.Format(context.Background(), src)
		if err != nil {
			t.Fatalf("Format returned error: %v", err)
		}
		if !bytes.Equal(got, src) {
			t.Fatalf("Format(cat) = %q, want identity %q", got, src)
		}
	})

	t.Run("nonexistent command errors", func(t *testing.T) {
		f := CmdFormatter{Name: "bage-no-such-formatter-xyz"}

		got, err := f.Format(context.Background(), []byte("x"))
		if err == nil {
			t.Fatalf("Format with nonexistent command: want error, got nil (out=%q)", got)
		}
		if got != nil {
			t.Fatalf("Format error path: want nil output, got %q", got)
		}
	})

	t.Run("context cancellation errors", func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		cancel()

		f := CmdFormatter{Name: "sleep", Args: []string{"5"}}
		if _, err := f.Format(ctx, []byte("x")); err == nil {
			t.Fatal("Format with cancelled context: want error, got nil")
		}
	})
}

// TestCmdLinter exercises the configured-command linter: a clean pass via
// "true" (exit 0 => nil), a blocking failure via "false" (non-zero => error),
// and context cancellation.
func TestCmdLinter(t *testing.T) {
	t.Run("true passes clean", func(t *testing.T) {
		l := CmdLinter{Name: "true"}
		if err := l.Lint(context.Background(), []byte("anything")); err != nil {
			t.Fatalf("Lint(true) = %v, want nil", err)
		}
	})

	t.Run("false fails and blocks", func(t *testing.T) {
		l := CmdLinter{Name: "false"}
		if err := l.Lint(context.Background(), []byte("anything")); err == nil {
			t.Fatal("Lint(false): want non-nil blocking error, got nil")
		}
	})

	t.Run("nonexistent command errors", func(t *testing.T) {
		l := CmdLinter{Name: "bage-no-such-linter-xyz"}
		if err := l.Lint(context.Background(), []byte("x")); err == nil {
			t.Fatal("Lint with nonexistent command: want error, got nil")
		}
	})

	t.Run("context cancellation errors", func(t *testing.T) {
		ctx, cancel := context.WithTimeout(context.Background(), time.Millisecond)
		defer cancel()

		l := CmdLinter{Name: "sleep", Args: []string{"5"}}
		if err := l.Lint(ctx, []byte("x")); err == nil {
			t.Fatal("Lint with expiring context: want error, got nil")
		}
	})
}
