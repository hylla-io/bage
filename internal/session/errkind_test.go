package session

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
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

func TestConflictErrorKind(t *testing.T) {
	drift := &ConflictError{Path: "a.go", Reason: "raw_hash drift", kind: KindDrift}
	if got := KindOf(drift); got != KindDrift {
		t.Errorf("KindOf(drift ConflictError) = %q, want %q", got, KindDrift)
	}
	conflict := &ConflictError{Path: "b.go", Reason: "conflict", kind: KindConflict}
	if got := KindOf(conflict); got != KindConflict {
		t.Errorf("KindOf(conflict ConflictError) = %q, want %q", got, KindConflict)
	}
	if !errors.Is(drift, ErrConflict) {
		t.Errorf("errors.Is(drift, ErrConflict) = false, want true")
	}
	if !errors.Is(conflict, ErrConflict) {
		t.Errorf("errors.Is(conflict, ErrConflict) = false, want true")
	}
}

func TestEnvelope(t *testing.T) {
	ce := &ConflictError{Path: "f", Reason: "conflict"}
	gotCE := Envelope(ce)
	wantCE := ErrorEnvelope{Kind: KindConflict, Path: "f", Message: ce.Error()}
	if gotCE != wantCE {
		t.Errorf("Envelope(ConflictError) = %+v, want %+v", gotCE, wantCE)
	}

	gotNF := Envelope(ErrNotFound)
	wantNF := ErrorEnvelope{Kind: KindNotFound, Path: "", Message: ErrNotFound.Error()}
	if gotNF != wantNF {
		t.Errorf("Envelope(ErrNotFound) = %+v, want %+v", gotNF, wantNF)
	}
}

func TestEnvelopeJSON(t *testing.T) {
	ce := &ConflictError{Path: "f", Reason: "conflict"}
	data, err := json.Marshal(Envelope(ce))
	if err != nil {
		t.Fatalf("json.Marshal(Envelope(ce)) error = %v", err)
	}
	var m map[string]any
	if err := json.Unmarshal(data, &m); err != nil {
		t.Fatalf("json.Unmarshal = %v", err)
	}
	for _, key := range []string{"kind", "path", "message"} {
		if _, ok := m[key]; !ok {
			t.Errorf("json missing key %q in %s", key, data)
		}
	}

	dataEmpty, err := json.Marshal(Envelope(ErrNotFound))
	if err != nil {
		t.Fatalf("json.Marshal(Envelope(ErrNotFound)) error = %v", err)
	}
	if strings.Contains(string(dataEmpty), "path") {
		t.Errorf("empty Path should omit 'path' key, got %s", dataEmpty)
	}
}

func TestEnvelopeRenderText(t *testing.T) {
	env := Envelope(&ConflictError{Path: "f", Reason: "conflict"})
	var buf bytes.Buffer
	if err := env.RenderText(&buf); err != nil {
		t.Fatalf("RenderText error = %v", err)
	}
	out := buf.String()
	if strings.Count(out, "\n") != 1 {
		t.Errorf("RenderText should write a single line, got %q", out)
	}
	if !strings.Contains(out, string(env.Kind)) {
		t.Errorf("RenderText output %q missing kind %q", out, env.Kind)
	}
	if !strings.Contains(out, env.Message) {
		t.Errorf("RenderText output %q missing message %q", out, env.Message)
	}
}
