// Package format defines the formatter and linter ports applied to staged
// edit content before it is committed, together with configured-command exec
// adapters and in-memory fakes for downstream tests.
//
// Per SPEC §3.5, formatting rewrites staged content and linting blocks the
// edit on failure: a non-nil Linter error means the staged content is
// rejected. Both ports are pure boundaries (interface-first, dependency
// inversion) so the edit pipeline never depends on a concrete tool.
package format

import "context"

// Formatter rewrites staged source content. Format returns the formatted
// bytes, or an error if the underlying tool fails; on error the staged
// content is left unchanged by the caller.
type Formatter interface {
	// Format returns the formatted form of src. The returned slice may alias
	// or replace src; callers should treat it as the new staged content.
	Format(ctx context.Context, src []byte) ([]byte, error)
}

// Linter validates staged source content. A nil return means the content is
// clean and the edit may proceed; a non-nil return is a lint failure that
// blocks the edit.
type Linter interface {
	// Lint reports whether src passes the configured checks. nil = clean.
	Lint(ctx context.Context, src []byte) error
}
