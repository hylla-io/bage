package session

import "errors"

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
