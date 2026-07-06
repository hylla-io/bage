//! A POSIX-atomic file writer: data is written to a temp file in the
//! target's directory, fsync'd, then renamed over the target.

use std::io::{self, Write as _};
use std::path::Path;

/// An atomic-write failure, tagged with the step that failed.
#[derive(Debug, thiserror::Error)]
#[error("atomicwrite: {op} {path:?}: {source}")]
pub struct AtomicWriteError {
    /// The step that failed: "create temp", "write temp", "fsync temp", or
    /// "rename".
    pub op: &'static str,
    /// The path involved in the failing step.
    pub path: String,
    #[source]
    pub source: io::Error,
}

/// Atomically writes `data` to `path`: create a temp file in the same
/// directory, write, fsync the temp file, then rename it over `path`. The
/// temp file is removed on any error (RAII — an unpersisted
/// `NamedTempFile` deletes itself on drop), so a failed write never leaves
/// partial state behind.
pub fn write(path: &Path, data: &[u8]) -> Result<(), AtomicWriteError> {
    let err = |op: &'static str, p: &Path, source: io::Error| AtomicWriteError {
        op,
        path: p.display().to_string(),
        source,
    };

    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{base}.tmp-"))
        .tempfile_in(dir)
        .map_err(|e| err("create temp in", dir, e))?;
    tmp.write_all(data)
        .map_err(|e| err("write temp", tmp.path(), e))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| err("fsync temp", tmp.path(), e))?;
    tmp.persist(path)
        .map_err(|e| err("rename over", path, e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        write(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        write(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        // No stray temp files remain.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "out.txt")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn failure_leaves_no_partial_state() {
        let dir = tempfile::tempdir().unwrap();
        let missing_dir = dir.path().join("nope");
        let target = missing_dir.join("out.txt");
        let e = write(&target, b"data").unwrap_err();
        assert_eq!(e.op, "create temp in");
        assert!(!target.exists());
    }
}
