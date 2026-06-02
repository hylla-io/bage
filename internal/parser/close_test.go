package parser

import "testing"

// countingCloser records how many times Close was invoked, standing in for a
// native engine handle whose own Close double-frees if called twice.
type countingCloser struct{ closed int }

func (c *countingCloser) Close() { c.closed++ }

// TestTreeCloseIdempotent verifies Tree.Close releases the native handle exactly
// once across repeated calls, and is a no-op for nil / no-native trees.
func TestTreeCloseIdempotent(t *testing.T) {
	cc := &countingCloser{}
	tr := &Tree{Native: cc}

	tr.Close()
	tr.Close()
	tr.Close()
	if cc.closed != 1 {
		t.Fatalf("native Close called %d times, want exactly 1", cc.closed)
	}
	if tr.Native != nil {
		t.Fatalf("Native not cleared after Close")
	}

	// No-op cases must not panic.
	(&Tree{}).Close()
	var nilTree *Tree
	nilTree.Close()
}
