package format

import (
	"bytes"
	"context"
	"errors"
	"testing"
)

// TestFakeFormatter covers the identity default and the configurable
// FormatFunc override, including its error path.
func TestFakeFormatter(t *testing.T) {
	t.Run("default is identity", func(t *testing.T) {
		f := FakeFormatter{}
		src := []byte("untouched")

		got, err := f.Format(context.Background(), src)
		if err != nil {
			t.Fatalf("Format returned error: %v", err)
		}
		if !bytes.Equal(got, src) {
			t.Fatalf("default Format = %q, want identity %q", got, src)
		}
	})

	t.Run("FormatFunc override", func(t *testing.T) {
		want := []byte("REWRITTEN")
		f := FakeFormatter{
			FormatFunc: func(_ context.Context, _ []byte) ([]byte, error) {
				return want, nil
			},
		}

		got, err := f.Format(context.Background(), []byte("in"))
		if err != nil {
			t.Fatalf("Format returned error: %v", err)
		}
		if !bytes.Equal(got, want) {
			t.Fatalf("override Format = %q, want %q", got, want)
		}
	})

	t.Run("FormatFunc error", func(t *testing.T) {
		sentinel := errors.New("boom")
		f := FakeFormatter{
			FormatFunc: func(_ context.Context, _ []byte) ([]byte, error) {
				return nil, sentinel
			},
		}

		if _, err := f.Format(context.Background(), nil); !errors.Is(err, sentinel) {
			t.Fatalf("Format error = %v, want %v", err, sentinel)
		}
	})
}

// TestFakeLinter covers the clean default, the configurable Err, and the
// LintFunc override.
func TestFakeLinter(t *testing.T) {
	t.Run("default is clean", func(t *testing.T) {
		l := FakeLinter{}
		if err := l.Lint(context.Background(), []byte("x")); err != nil {
			t.Fatalf("default Lint = %v, want nil", err)
		}
	})

	t.Run("Err blocks", func(t *testing.T) {
		sentinel := errors.New("lint failure")
		l := FakeLinter{Err: sentinel}
		if err := l.Lint(context.Background(), []byte("x")); !errors.Is(err, sentinel) {
			t.Fatalf("Lint = %v, want %v", err, sentinel)
		}
	})

	t.Run("LintFunc override", func(t *testing.T) {
		sentinel := errors.New("dynamic")
		l := FakeLinter{
			Err: errors.New("ignored when LintFunc set"),
			LintFunc: func(_ context.Context, _ []byte) error {
				return sentinel
			},
		}
		if err := l.Lint(context.Background(), []byte("x")); !errors.Is(err, sentinel) {
			t.Fatalf("Lint = %v, want %v", err, sentinel)
		}
	})
}
