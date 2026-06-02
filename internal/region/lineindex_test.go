package region

import "testing"

func TestLineIndexByteForLineRoundTrip(t *testing.T) {
	src := []byte("alpha\nbeta\ngamma\n")
	li := NewLineIndex(src)

	// Lines: 1="alpha" @0, 2="beta" @6, 3="gamma" @11, 4="" @17 (after final \n).
	cases := []struct {
		line int
		off  int
	}{
		{1, 0}, {2, 6}, {3, 11}, {4, 17},
	}
	for _, c := range cases {
		if got := li.ByteForLine(c.line); got != c.off {
			t.Fatalf("ByteForLine(%d) = %d, want %d", c.line, got, c.off)
		}
		// Round-trip: the byte at a line start maps back to that line.
		if got := li.LineForByte(c.off); got != c.line {
			t.Fatalf("LineForByte(%d) = %d, want line %d", c.off, got, c.line)
		}
	}
}

func TestLineIndexClamps(t *testing.T) {
	src := []byte("a\nb")
	li := NewLineIndex(src)
	if got := li.ByteForLine(0); got != 0 {
		t.Fatalf("ByteForLine(0) = %d, want 0 (clamp low)", got)
	}
	if got := li.ByteForLine(-5); got != 0 {
		t.Fatalf("ByteForLine(-5) = %d, want 0 (clamp low)", got)
	}
	if got := li.ByteForLine(99); got != len(src) {
		t.Fatalf("ByteForLine(99) = %d, want %d (clamp high)", got, len(src))
	}
	if got := li.LineForByte(-1); got != 1 {
		t.Fatalf("LineForByte(-1) = %d, want 1 (clamp low)", got)
	}
	if got := li.LineForByte(1000); got != li.Lines() {
		t.Fatalf("LineForByte(1000) = %d, want last line %d", got, li.Lines())
	}
}

func TestLineIndexCRLF(t *testing.T) {
	// CRLF: the '\r' stays part of the line's bytes; line starts follow '\n'.
	src := []byte("one\r\ntwo\r\nthree")
	li := NewLineIndex(src)

	// "one\r\n" = 5 bytes → line 2 starts at 5; "two\r\n" = 5 → line 3 at 10.
	if got := li.ByteForLine(2); got != 5 {
		t.Fatalf("CRLF ByteForLine(2) = %d, want 5", got)
	}
	if got := li.ByteForLine(3); got != 10 {
		t.Fatalf("CRLF ByteForLine(3) = %d, want 10", got)
	}
	// The '\r' (byte 3) is on line 1; the 't' of "two" (byte 5) is on line 2.
	if got := li.LineForByte(3); got != 1 {
		t.Fatalf("CRLF LineForByte(3=\\r) = %d, want 1", got)
	}
	if got := li.LineForByte(5); got != 2 {
		t.Fatalf("CRLF LineForByte(5='t') = %d, want 2", got)
	}
	// Byte column of 't' on line 2 is 0 (byte cols, CRLF-immune).
	if line, col := li.PositionForByte(5); line != 2 || col != 0 {
		t.Fatalf("PositionForByte(5) = (%d,%d), want (2,0)", line, col)
	}
}

func TestLineIndexMultibyteByteCols(t *testing.T) {
	// "héllo X": h(0) é(1,2 — 2 bytes) l(3) l(4) o(5) space(6) X(7). The 'X'
	// sits at byte col 7, not rune col 6. Byte-col convention per
	// HYLLA_NODE_CONTRACT §1.
	src := []byte("héllo X\nnext\n")
	li := NewLineIndex(src)

	idxX := 7
	if src[idxX] != 'X' {
		t.Fatalf("test bug: byte %d = %q, want X", idxX, src[idxX])
	}
	line, col := li.PositionForByte(idxX)
	if line != 1 || col != 7 {
		t.Fatalf("PositionForByte(X) = (%d,%d), want (1,7) byte-col", line, col)
	}

	// "héllo X\n" = h(1)é(2)l(1)l(1)o(1) (1)X(1)\n(1) = 9 bytes → line 2 @ 9.
	if got := li.ByteForLine(2); got != 9 {
		t.Fatalf("ByteForLine(2) = %d, want 9", got)
	}
}

func TestFillLineCols(t *testing.T) {
	src := []byte("ab\ncdef\ngh\n")
	li := NewLineIndex(src)
	// Region over "cdef" = bytes [3:7].
	r := Region{StartByte: 3, EndByte: 7}
	got := li.FillLineCols(r)
	if got.StartLine != 2 || got.StartCol != 0 {
		t.Fatalf("start = (%d,%d), want (2,0)", got.StartLine, got.StartCol)
	}
	// EndByte 7 is the position AT byte 7 = end of line 2 (the '\n' at 7), col 4.
	if got.EndLine != 2 || got.EndCol != 4 {
		t.Fatalf("end = (%d,%d), want (2,4)", got.EndLine, got.EndCol)
	}
}

func TestResolveLines(t *testing.T) {
	src := []byte("alpha\nbeta\ngamma\ndelta\n")
	li := NewLineIndex(src)

	// Line-addressed region covering lines 2..3 ("beta\ngamma\n").
	r := Region{StartByte: LineSentinel, StartLine: 2, EndLine: 3}
	got := li.ResolveLines(r)

	if got.StartByte != 6 {
		t.Fatalf("StartByte = %d, want 6 (start of line 2)", got.StartByte)
	}
	// End is the start of line 4 (after EndLine 3) = byte 17.
	if got.EndByte != 17 {
		t.Fatalf("EndByte = %d, want 17 (start of line 4)", got.EndByte)
	}
	if body := string(src[got.StartByte:got.EndByte]); body != "beta\ngamma\n" {
		t.Fatalf("resolved body = %q, want %q", body, "beta\ngamma\n")
	}
	// Line/col fields are refreshed.
	if got.StartLine != 2 || got.StartCol != 0 {
		t.Fatalf("refreshed start = (%d,%d), want (2,0)", got.StartLine, got.StartCol)
	}
}

func TestResolveLinesNoOpWhenByteAddressed(t *testing.T) {
	li := NewLineIndex([]byte("x\ny\n"))
	r := Region{StartByte: 0, EndByte: 2, StartLine: 1, EndLine: 1}
	got := li.ResolveLines(r)
	if got.StartByte != 0 || got.EndByte != 2 {
		t.Fatalf("byte-addressed region mutated: [%d:%d]", got.StartByte, got.EndByte)
	}
}
