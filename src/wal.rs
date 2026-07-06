//! A durable, file-based write-ahead log of edit intents. Each append fsyncs
//! one JSON-encoded record (one line) to `<dir>/wal.log`; replay reads every
//! record back in order. There is no SQLite or external store: the log is a
//! plain append-only file so a crash mid-edit can be reconciled on restart
//! by replaying the recorded intents.
//!
//! Records are JSON-compatible with the Go implementation (same field names,
//! `[]byte` originals as base64), so a WAL written by either side replays in
//! the other.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write as _};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::edit::FileEdit;

/// The fixed file name of the WAL within its directory.
const LOG_NAME: &str = "wal.log";

/// One durably-recorded edit intent. It captures the edits to apply, the
/// original bytes of each affected file (for restore-on-failure), and the
/// expected raw and normalized content hashes per file (for drift detection)
/// so a recovering process has everything needed to reapply or roll back.
/// Go marshals nil slices/maps as JSON `null`; this decodes `null` (or an
/// absent key, via `default`) to the empty container so Go-written records
/// replay cleanly.
fn null_default<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    /// Uniquely identifies this intent.
    #[serde(default)]
    pub id: String,
    /// The byte-range replacements this intent will apply.
    #[serde(default, deserialize_with = "null_default")]
    pub edits: Vec<FileEdit>,
    /// Maps each affected file path to its pre-edit raw bytes (base64 on the
    /// wire, matching Go's `[]byte` JSON encoding).
    #[serde(default, with = "base64_map")]
    pub originals: HashMap<String, Vec<u8>>,
    /// Maps each file path to its expected raw content hash.
    #[serde(default, deserialize_with = "null_default")]
    pub expected_raw_hash: HashMap<String, String>,
    /// Maps each file path to its expected normalized hash.
    #[serde(default, deserialize_with = "null_default")]
    pub expected_norm_hash: HashMap<String, String>,
    /// The paths this intent is creating from non-existence. On crash
    /// recovery or rollback each path is unlinked, undoing a half-created
    /// file (create's undo is unlink, not a content restore) (ADR-0004).
    /// Absent from the wire when empty so old records keep decoding.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub creates: Vec<String>,
    /// The paths this intent is removing — the inverse of `creates`: a
    /// delete's undo is a content RESTORE, so each deleted path's FULL prior
    /// bytes are captured in `originals` before the unlink, and a crash or
    /// rollback restores them from there (ADR-0004).
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub deletes: Vec<String>,
    /// The `{from, to}` relocations this intent is performing. A move is a
    /// DELETE(from)+CREATE(to) as one atomic-on-recovery unit (ADR-0004):
    /// the source bytes are captured in `originals[from]` BEFORE the
    /// destination is claimed and the source unlinked, so a crash converges
    /// to fully-moved or fully-original and the source bytes are never
    /// lost.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub moves: Vec<Move>,
    /// Marks a UNIFIED BATCH intent (`apply_batch`, ADR-0004 §10.1): a
    /// heterogeneous op list applied as ONE all-or-nothing change. Its
    /// recovery model is INTERNALLY ONE-DIRECTIONAL — recovery converges a
    /// batch intent fully BACKWARD (to the pre-batch state), so a move
    /// inside a batch is UNDONE (restore `originals[from]` at `from`, remove
    /// `to`) in the same backward direction as the batch's
    /// edits/deletes/creates, never converged forward. A single-op move
    /// leaves this `false` and keeps its own FORWARD-converge semantics, so
    /// a batch can never produce the half-applied state where the move
    /// completes while the other ops roll back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub batch: bool,
}

/// One source→destination relocation recorded in an [`Intent`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    /// The source path the move removes.
    pub from: String,
    /// The destination path the move creates with the source bytes.
    pub to: String,
}

/// Serde adapter encoding a `HashMap<String, Vec<u8>>` with base64 string
/// values, byte-compatible with Go's `map[string][]byte` JSON encoding.
mod base64_map {
    use std::collections::HashMap;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        map: &HashMap<String, Vec<u8>>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        let encoded: HashMap<&str, String> = map
            .iter()
            .map(|(k, v)| (k.as_str(), STANDARD.encode(v)))
            .collect();
        encoded.serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<HashMap<String, Vec<u8>>, D::Error> {
        // Option-tolerant: Go marshals a nil map as JSON null.
        let encoded: HashMap<String, String> = Option::deserialize(de)?.unwrap_or_default();
        encoded
            .into_iter()
            .map(|(k, v)| {
                STANDARD
                    .decode(v.as_bytes())
                    .map(|b| (k, b))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// A WAL failure.
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("wal: {op} {path:?}: {source}")]
    Io {
        op: &'static str,
        path: String,
        source: io::Error,
    },
    #[error("wal: marshal intent {id:?}: {source}")]
    Marshal {
        id: String,
        source: serde_json::Error,
    },
}

fn io_err(op: &'static str, path: &Path, source: io::Error) -> WalError {
    WalError::Io {
        op,
        path: path.display().to_string(),
        source,
    }
}

/// Durably records one intent. It creates `dir` if needed, then opens
/// `<dir>/wal.log` in append mode, writes the JSON encoding of `intent` as a
/// single line, and fsyncs the file before returning so the record survives
/// a crash.
pub fn append(dir: &Path, intent: &Intent) -> Result<(), WalError> {
    std::fs::create_dir_all(dir).map_err(|e| io_err("create dir", dir, e))?;

    let mut line = serde_json::to_vec(intent).map_err(|e| WalError::Marshal {
        id: intent.id.clone(),
        source: e,
    })?;
    line.push(b'\n');

    let path = dir.join(LOG_NAME);
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| io_err("open", &path, e))?;
    f.write_all(&line).map_err(|e| io_err("write", &path, e))?;
    f.sync_all().map_err(|e| io_err("fsync", &path, e))?;

    // Fsync the PARENT DIRECTORY too. Syncing the file content alone does
    // not make the directory entry durable: when wal.log was just created, a
    // power-loss crash could lose the directory entry and thus the whole
    // record, even though sync_all returned — leaving a caller that already
    // acted on append's success (e.g. delete, which unlinks the target right
    // after) with UNRECOVERABLE bytes. Syncing the directory closes that
    // window, so the WAL-before-unlink ordering is a true crash guarantee
    // for delete, and the same hardening strengthens the create and edit
    // paths that share append.
    let d = File::open(dir).map_err(|e| io_err("open dir", dir, e))?;
    d.sync_all().map_err(|e| io_err("sync dir", dir, e))?;
    Ok(())
}

/// Reads every recorded intent from `<dir>/wal.log` in append order. A
/// missing directory or log file is treated as an empty log.
pub fn replay(dir: &Path) -> Result<Vec<Intent>, WalError> {
    let path = dir.join(LOG_NAME);
    let f = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err("open", &path, e)),
    };

    let mut out = Vec::new();
    for line in BufReader::new(f).split(b'\n') {
        let line = line.map_err(|e| io_err("scan", &path, e))?;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Intent>(&line) {
            Ok(intent) => out.push(intent),
            // A record that fails to decode is treated as a torn trailing
            // write: the process crashed mid-append before this record was
            // fully written/fsynced. Stop and return every cleanly-committed
            // record before it, so crash recovery preserves all good intents
            // (SPEC §1.2). Append writes one fsynced line at a time, so only
            // the final record can be torn.
            Err(_) => break,
        }
    }
    Ok(out)
}

/// Removes the WAL log file, if present. Called after a fully-successful
/// commit or a completed recovery.
pub fn clear(dir: &Path) -> Result<(), WalError> {
    let path = dir.join(LOG_NAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove", &path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> Intent {
        Intent {
            id: id.into(),
            edits: vec![FileEdit {
                path: "a.txt".into(),
                start_byte: 0,
                end_byte: 3,
                new_text: "xyz".into(),
            }],
            originals: HashMap::from([("a.txt".to_string(), b"abc\xff".to_vec())]),
            expected_raw_hash: HashMap::from([("a.txt".to_string(), "0".repeat(16))]),
            expected_norm_hash: HashMap::from([("a.txt".to_string(), "1".repeat(16))]),
            ..Default::default()
        }
    }

    #[test]
    fn append_replay_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let a = sample("i1");
        let mut b = sample("i2");
        b.creates = vec!["new.txt".into()];
        b.moves = vec![Move {
            from: "old.txt".into(),
            to: "new2.txt".into(),
        }];
        b.batch = true;
        append(dir.path(), &a).unwrap();
        append(dir.path(), &b).unwrap();
        let got = replay(dir.path()).unwrap();
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn replay_missing_log_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(replay(dir.path()).unwrap().is_empty());
        assert!(replay(&dir.path().join("nonexistent")).unwrap().is_empty());
    }

    #[test]
    fn replay_tolerates_torn_trailing_write() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("good")).unwrap();
        // Simulate a crash mid-append: a truncated JSON line at the tail.
        let path = dir.path().join(LOG_NAME);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":\"torn").unwrap();
        let got = replay(dir.path()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "good");
    }

    #[test]
    fn wire_format_matches_go() {
        // omitempty parity: empty lifecycle fields stay OFF the wire; the
        // originals map is base64.
        let j = serde_json::to_value(sample("i1")).unwrap();
        assert_eq!(j["id"], "i1");
        assert_eq!(j["originals"]["a.txt"], "YWJj/w==");
        assert!(j.get("creates").is_none());
        assert!(j.get("deletes").is_none());
        assert!(j.get("moves").is_none());
        assert!(j.get("batch").is_none());
        assert_eq!(j["edits"][0]["StartByte"], 0);
        // And a Go-written record (absent optional keys) decodes cleanly.
        let go_record = r#"{"id":"g","edits":null,"originals":{"p":"aGk="},"expected_raw_hash":{},"expected_norm_hash":{}}"#;
        let intent: Intent = serde_json::from_str(go_record).unwrap();
        assert_eq!(intent.id, "g");
        assert_eq!(intent.originals["p"], b"hi");
        assert!(!intent.batch);
    }

    #[test]
    fn clear_removes_log_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("i1")).unwrap();
        clear(dir.path()).unwrap();
        assert!(replay(dir.path()).unwrap().is_empty());
        clear(dir.path()).unwrap();
    }
}
