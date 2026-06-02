package format

import (
	"bytes"
	"context"
	"fmt"
	"os/exec"
)

// CmdFormatter is a Formatter backed by an external command. The command is
// invoked with src piped to stdin and its stdout taken as the formatted
// result; a non-zero exit is reported as an error including stderr.
type CmdFormatter struct {
	// Name is the executable to run (resolved via PATH).
	Name string
	// Args are the arguments passed to the executable.
	Args []string
}

// Format runs the configured command with src on stdin and returns its
// stdout. A non-zero exit, or any failure to launch the command, yields an
// error wrapping the command name and the captured stderr.
func (f CmdFormatter) Format(ctx context.Context, src []byte) ([]byte, error) {
	cmd := exec.CommandContext(ctx, f.Name, f.Args...)
	cmd.Stdin = bytes.NewReader(src)

	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("format: %s: %w: %s", f.Name, err, stderr.String())
	}
	return stdout.Bytes(), nil
}

// CmdLinter is a Linter backed by an external command. The command is invoked
// with src piped to stdin; a zero exit means clean (nil), while a non-zero
// exit (or launch failure) is a blocking lint failure including stderr.
type CmdLinter struct {
	// Name is the executable to run (resolved via PATH).
	Name string
	// Args are the arguments passed to the executable.
	Args []string
}

// Lint runs the configured command with src on stdin. It returns nil when the
// command exits zero, and an error wrapping the command name and captured
// stderr otherwise.
func (l CmdLinter) Lint(ctx context.Context, src []byte) error {
	cmd := exec.CommandContext(ctx, l.Name, l.Args...)
	cmd.Stdin = bytes.NewReader(src)

	var stderr bytes.Buffer
	cmd.Stderr = &stderr

	if err := cmd.Run(); err != nil {
		return fmt.Errorf("lint: %s: %w: %s", l.Name, err, stderr.String())
	}
	return nil
}
