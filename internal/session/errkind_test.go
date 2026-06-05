package session

import (
	"errors"
	"fmt"
	"testing"
)

// driftErr is a local Kinded error used to verify KindOf prefers an
// errors.As-discoverable Kind() over the sentinel switch.
type driftErr struct{}

func (driftErr) Error() string { return "drift" }
func (driftErr) Kind() Kind    { return KindDrift }

func TestKindOf(t *testing.T) {
	tests := []struct {
		name string
		err  error
		want Kind
	}{
		{"conflict", ErrConflict, KindConflict},
		{"exists", ErrExists, KindExists},
		{"not-found", ErrNotFound, KindNotFound},
		{"wrapped not-found", fmt.Errorf("wrap: %w", ErrNotFound), KindNotFound},
		{"default io", errors.New("x"), KindIO},
		{"nil", nil, Kind("")},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := KindOf(tt.err); got != tt.want {
				t.Errorf("KindOf(%v) = %q, want %q", tt.err, got, tt.want)
			}
		})
	}
}

func TestKindedInterface(t *testing.T) {
	err := fmt.Errorf("wrap: %w", driftErr{})
	if got := KindOf(err); got != KindDrift {
		t.Errorf("KindOf(driftErr) = %q, want %q", got, KindDrift)
	}
}
