// Package atomicwrite provides a POSIX-atomic file writer: data is written to
// a temp file in the target's directory, fsync'd, then renamed over the target.
package atomicwrite

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
)

// Write atomically writes data to path. It creates a temp file in the same
// directory as path, writes data, fsyncs the temp file before closing it, then
// renames the temp file over path. The temp file is removed on any error so a
// failed write never leaves partial state behind.
func Write(path string, data []byte) (err error) {
	dir := filepath.Dir(path)

	tmp, err := os.CreateTemp(dir, "."+filepath.Base(path)+".tmp-*")
	if err != nil {
		return fmt.Errorf("atomicwrite: create temp in %q: %w", dir, err)
	}
	tmpName := tmp.Name()

	// On any error after creation, close and remove the temp file.
	defer func() {
		if err != nil {
			tmp.Close()
			os.Remove(tmpName)
		}
	}()

	if _, err = writeAll(tmp, data); err != nil {
		return fmt.Errorf("atomicwrite: write temp %q: %w", tmpName, err)
	}
	if err = tmp.Sync(); err != nil {
		return fmt.Errorf("atomicwrite: fsync temp %q: %w", tmpName, err)
	}
	if err = tmp.Close(); err != nil {
		return fmt.Errorf("atomicwrite: close temp %q: %w", tmpName, err)
	}
	if err = os.Rename(tmpName, path); err != nil {
		return fmt.Errorf("atomicwrite: rename %q -> %q: %w", tmpName, path, err)
	}
	return nil
}

// writeAll writes all of data to w, returning the number of bytes written and
// any error. It exists to keep Write's error handling on a single io boundary.
func writeAll(w io.Writer, data []byte) (int, error) {
	return w.Write(data)
}
