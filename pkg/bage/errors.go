package bage

import "github.com/hylla-io/bage/internal/session"

// Kind is the stable, machine-readable classification of a Båge error, re-exported
// so an external host (Hylla, an MCP server) can react to a failure without
// importing internal/session or inspecting wrapped chains. See session.Kind.
type Kind = session.Kind

// ErrorEnvelope is the machine- and human-renderable projection of a Båge error:
// its stable Kind, the offending path (when known), and the underlying message.
// Hosts marshal it to JSON or render it as one diagnostic line. See
// session.ErrorEnvelope.
type ErrorEnvelope = session.ErrorEnvelope

// Re-exported error-classification kinds. Each names a stable Kind a host can
// switch on; see session for the canonical definitions.
const (
	// KindConflict marks a region-anchored edit that could not be resolved against
	// the live file (concurrent change or ambiguous twins).
	KindConflict = session.KindConflict
	// KindDrift marks a raw_hash drift reject: the live bytes no longer match the
	// expected anchor the caller saw.
	KindDrift = session.KindDrift
	// KindExists marks a create rejected because the target path already exists.
	KindExists = session.KindExists
	// KindNotFound marks an op rejected because the target path does not exist.
	KindNotFound = session.KindNotFound
	// KindUsage marks a caller/usage error (bad arguments or invalid request).
	KindUsage = session.KindUsage
	// KindIO marks the default I/O or otherwise unclassified failure.
	KindIO = session.KindIO
)

// KindOf classifies err into a Kind: the empty Kind for nil, a self-classifying
// error's own Kind, the matched sentinel for ErrConflict/ErrExists/ErrNotFound,
// and otherwise KindIO. It thin-wraps session.KindOf.
func KindOf(err error) Kind { return session.KindOf(err) }

// Envelope projects err into an ErrorEnvelope: it classifies err via KindOf,
// records err.Error() as the message, and lifts the Path from a wrapped
// ConflictError when present. It thin-wraps session.Envelope.
func Envelope(err error) ErrorEnvelope { return session.Envelope(err) }
