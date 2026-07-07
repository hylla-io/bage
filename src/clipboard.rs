//! The single-slot file clipboard backing `bage cut/copy/paste --clip`: a
//! JSON record written atomically so a cut in one invocation (or process)
//! can be pasted by another. The slot lives at `$BAGE_CLIPBOARD` when set,
//! else `$HOME/.bage/clipboard.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::atomicwrite;

/// One clipboard slot: the content plus provenance so a paste (or a human
/// inspecting the file) can see where the bytes came from and whether the
/// source region was removed (`cut`) or left in place (`copy`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clip {
    /// The copied/cut bytes (lossy UTF-8, matching block content semantics).
    pub content: String,
    /// The file the content came from.
    pub source_path: String,
    /// The region_hash of the source region at capture time.
    pub region_hash: String,
    /// Whether the source region was removed (`cut`) or copied.
    pub cut: bool,
}

/// A clipboard failure.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("clipboard is empty (nothing cut or copied with --clip yet)")]
    Empty,
    #[error("clipboard: {op} {path:?}: {message}")]
    Io {
        op: &'static str,
        path: String,
        message: String,
    },
}

/// The clipboard slot path: `$BAGE_CLIPBOARD` when set, else
/// `$HOME/.bage/clipboard.json` (falling back to the OS temp dir when HOME
/// is unset).
pub fn slot_path() -> PathBuf {
    if let Ok(p) = std::env::var("BAGE_CLIPBOARD")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h).join(".bage").join("clipboard.json"),
        _ => std::env::temp_dir().join("bage-clipboard.json"),
    }
}

/// Atomically writes `clip` to the slot, creating the parent directory if
/// needed.
pub fn write(clip: &Clip) -> Result<(), ClipboardError> {
    let path = slot_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ClipboardError::Io {
            op: "create dir for",
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
    }
    let json = serde_json::to_vec_pretty(clip).map_err(|e| ClipboardError::Io {
        op: "encode",
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    atomicwrite::write(&path, &json).map_err(|e| ClipboardError::Io {
        op: "write",
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

/// Reads the slot; a missing slot is [`ClipboardError::Empty`].
pub fn read() -> Result<Clip, ClipboardError> {
    let path = slot_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ClipboardError::Empty),
        Err(e) => {
            return Err(ClipboardError::Io {
                op: "read",
                path: path.display().to_string(),
                message: e.to_string(),
            });
        }
    };
    serde_json::from_slice(&bytes).map_err(|e| ClipboardError::Io {
        op: "decode",
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes access to the process-wide BAGE_CLIPBOARD env var across
    /// the tests in this module.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_slot<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("clip.json");
        // SAFETY: guarded by ENV_LOCK; tests in this module are the only
        // readers of this variable.
        unsafe { std::env::set_var("BAGE_CLIPBOARD", &slot) };
        let out = f();
        unsafe { std::env::remove_var("BAGE_CLIPBOARD") };
        out
    }

    #[test]
    fn round_trips_a_clip() {
        with_slot(|| {
            let clip = Clip {
                content: "fn x() {}\n".into(),
                source_path: "a.rs".into(),
                region_hash: "0".repeat(16),
                cut: true,
            };
            write(&clip).unwrap();
            assert_eq!(read().unwrap(), clip);
            // Overwrite wins.
            let clip2 = Clip {
                cut: false,
                ..clip.clone()
            };
            write(&clip2).unwrap();
            assert_eq!(read().unwrap(), clip2);
        });
    }

    #[test]
    fn empty_slot_is_a_distinct_error() {
        with_slot(|| {
            assert!(matches!(read(), Err(ClipboardError::Empty)));
        });
    }
}
