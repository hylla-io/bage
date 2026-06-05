package session

import (
	"errors"
	"fmt"
	"io"
)

// Kind is a stable, machine-readable classification of a session error, used by
// hosts (CLI exit codes, Hylla) to react to a failure without inspecting wrapped
// error chains or message text.
type Kind string

const (
	// KindConflict marks a region-anchored edit that could not be resolved against
	// the live file (concurrent change or ambiguous twins); see ErrConflict.
	KindConflict Kind = "conflict"
	// KindDrift marks a raw_hash drift reject: the live bytes no longer match the
	// expected anchor the caller saw.
	KindDrift Kind = "drift"
	// KindExists marks a create rejected because the target path already exists;
	// see ErrExists.
	KindExists Kind = "exists"
	// KindNotFound marks an op rejected because the target path does not exist;
	// see ErrNotFound.
	KindNotFound Kind = "not-found"
	// KindUsage marks a caller/usage error (bad arguments or invalid request).
	KindUsage Kind = "usage"
	// KindIO marks the default I/O or otherwise unclassified failure.
	KindIO Kind = "io"
)

// Kinded is implemented by errors that classify themselves. KindOf prefers an
// errors.As-discoverable Kinded value over the sentinel switch.
type Kinded interface {
	Kind() Kind
}

// KindOf classifies err into a Kind. It returns the empty Kind for a nil error,
// the error's own Kind if any error in the chain implements Kinded, then matches
// the known sentinels (ErrConflict, ErrExists, ErrNotFound) via errors.Is, and
// otherwise defaults to KindIO.
func KindOf(err error) Kind {
	if err == nil {
		return ""
	}
	var k Kinded
	if errors.As(err, &k) {
		return k.Kind()
	}
	switch {
	case errors.Is(err, ErrConflict):
		return KindConflict
	case errors.Is(err, ErrExists):
		return KindExists
	case errors.Is(err, ErrNotFound):
		return KindNotFound
	default:
		return KindIO
	}
}

// ErrorEnvelope is the machine- and human-renderable projection of a session
// error: its stable Kind, the offending path (when known), and the underlying
// error message. Hosts marshal it to JSON for Hylla or render it as a single
// diagnostic line for the CLI via RenderText.
type ErrorEnvelope struct {
	// Kind is the stable classification from KindOf.
	Kind Kind `json:"kind"`
	// Path is the file the failure concerns, omitted from JSON when empty.
	Path string `json:"path,omitempty"`
	// Message is the underlying error's text.
	Message string `json:"message"`
}

// Envelope projects err into an ErrorEnvelope: it classifies err via KindOf,
// records err.Error() as the message, and lifts the Path from a wrapped
// *ConflictError when one is present in the chain.
func Envelope(err error) ErrorEnvelope {
	env := ErrorEnvelope{Kind: KindOf(err), Message: err.Error()}
	if ce := (*ConflictError)(nil); errors.As(err, &ce) {
		env.Path = ce.Path
	}
	return env
}

// RenderText writes the envelope as a single "bage: <kind>: <message>" line to
// w. It implements the render.TextRenderable contract by method shape alone, so
// this package imports only io and fmt — never pkg/render.
func (e ErrorEnvelope) RenderText(w io.Writer) error {
	_, err := fmt.Fprintf(w, "bage: %s: %s\n", e.Kind, e.Message)
	return err
}
