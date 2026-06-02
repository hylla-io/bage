package format

import "context"

// FakeFormatter is an in-memory Formatter for tests. When FormatFunc is set it
// is invoked; otherwise Format behaves as the identity transform, returning
// src unchanged.
type FakeFormatter struct {
	// FormatFunc, when non-nil, fully determines Format's behaviour.
	FormatFunc func(ctx context.Context, src []byte) ([]byte, error)
}

// Format delegates to FormatFunc when set, and otherwise returns src
// unchanged (identity formatting).
func (f FakeFormatter) Format(ctx context.Context, src []byte) ([]byte, error) {
	if f.FormatFunc != nil {
		return f.FormatFunc(ctx, src)
	}
	return src, nil
}

// FakeLinter is an in-memory Linter for tests. When LintFunc is set it is
// invoked; otherwise Lint returns Err (nil by default, meaning clean).
type FakeLinter struct {
	// LintFunc, when non-nil, fully determines Lint's behaviour.
	LintFunc func(ctx context.Context, src []byte) error
	// Err is returned by Lint when LintFunc is nil. nil means clean.
	Err error
}

// Lint delegates to LintFunc when set, and otherwise returns Err.
func (l FakeLinter) Lint(ctx context.Context, src []byte) error {
	if l.LintFunc != nil {
		return l.LintFunc(ctx, src)
	}
	return l.Err
}
