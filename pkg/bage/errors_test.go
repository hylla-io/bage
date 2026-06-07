package bage_test

import (
	"errors"
	"testing"

	"github.com/hylla-io/bage/pkg/bage"
)

// TestErrorTaxonomyReExport proves the session error taxonomy is reachable
// through ONLY the public pkg/bage surface — no internal/session import — so an
// external host (Hylla, an MCP server) can classify and envelope failures.
func TestErrorTaxonomyReExport(t *testing.T) {
	// The Kind consts are usable as bage.Kind values.
	kinds := map[string]bage.Kind{
		"conflict":  bage.KindConflict,
		"drift":     bage.KindDrift,
		"exists":    bage.KindExists,
		"not-found": bage.KindNotFound,
		"usage":     bage.KindUsage,
		"io":        bage.KindIO,
	}
	for want, got := range kinds {
		if string(got) != want {
			t.Errorf("Kind const = %q, want %q", string(got), want)
		}
	}

	// A nil error classifies to the empty Kind.
	if k := bage.KindOf(nil); k != bage.Kind("") {
		t.Errorf("KindOf(nil) = %q, want empty", string(k))
	}

	// An unclassified, session-free error defaults to KindIO — proving the
	// taxonomy is reachable WITHOUT importing internal/session.
	plain := errors.New("x")
	if k := bage.KindOf(plain); k != bage.KindIO {
		t.Errorf("KindOf(plain) = %q, want %q", string(k), string(bage.KindIO))
	}

	// Envelope projects the same plain error into a usable ErrorEnvelope.
	env := bage.Envelope(plain)
	if env.Kind != bage.KindIO {
		t.Errorf("Envelope(plain).Kind = %q, want %q", string(env.Kind), string(bage.KindIO))
	}
	if env.Message != "x" {
		t.Errorf("Envelope(plain).Message = %q, want %q", env.Message, "x")
	}
	if env.Path != "" {
		t.Errorf("Envelope(plain).Path = %q, want empty", env.Path)
	}
}
