//! A durable, file-based write-ahead log of edit intents. There is no SQLite
//! or external store: each intent is a plain append-only file so a crash
//! mid-edit can be reconciled on restart.
//!
//! ONE FILE PER INTENT, `<dir>/bage-intent-<hex id>.wal`. Clearing an intent
//! unlinks its own file and nothing else, so finishing one op can never drop
//! another op's in-flight record — a shared log cleared as a whole did
//! exactly that. The file's first line is the intent, JSON-compatible with
//! the Go implementation (same field names, `[]byte` originals as base64).
//! Later lines are versioned records (`"v":2`) the owning op appends:
//!
//! - `{"v":2,"applying":{"path":P,"after":H,"before":B64?}}` — written
//!   BEFORE a commit writes `P`: `after` is the raw hash the write leaves,
//!   `before` the bytes it replaces when they differ from `originals[P]`.
//! - `{"v":2,"landed":true}` — written once every byte of the op is durable
//!   and before the file is cleared. It is what tells recovery "landed, the
//!   clear failed: keep" from "never finished: undo". Without it the two
//!   states read the same, and undoing destroys committed work.
//!
//! Lifecycle intents (create/delete/move/batch) carry their after-images in
//! the intent itself (`after`), since they are known before anything is
//! written. Recovery undoes a path only while it still holds exactly what
//! this intent left there, so a later write by anyone else is never undone.
//!
//! `<dir>/wal.log`, the shared log older versions wrote, is still replayed:
//! its intents carry no after-images and recover them by the old rules.
//!
//! A torn trailing line reads as absent: the crash came before it was
//! durable. Anything else the reader cannot place — an unknown `v`, a
//! misshapen record, a record after the landed marker, a file whose name
//! does not match its intent — is a loud [`WalError`], never a guess about
//! which bytes are current.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::edit::FileEdit;
use crate::fault;

/// The shared log older versions wrote; replayed, never written.
const LEGACY_LOG: &str = "wal.log";
/// Per-intent file name parts. The prefix keeps a shared WAL dir (the CLI
/// uses the system temp dir) from reading other programs' files.
const FILE_PREFIX: &str = "bage-intent-";
const FILE_SUFFIX: &str = ".wal";
/// The format version of every non-intent record.
const RECORD_VERSION: u64 = 2;

/// Maps each path an op touches to the raw hash of the bytes it leaves
/// there; `None` = the op leaves the path absent.
pub type AfterImages = BTreeMap<String, Option<String>>;

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

/// One durably-recorded edit intent. It captures the edits to apply, the
/// original bytes of each affected file (for restore-on-failure), and the
/// expected raw and normalized content hashes per file (for drift detection)
/// so a recovering process has everything needed to reapply or roll back.
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
    /// What a lifecycle intent leaves at each path it touches. Recovery
    /// acts on a path only while it holds exactly this, so it never undoes
    /// a change someone made after the op. Empty for a prepared edit plan,
    /// whose after-images are `applying` records written at commit.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub after: AfterImages,
}

/// One source→destination relocation recorded in an [`Intent`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    /// The source path the move removes.
    pub from: String,
    /// The destination path the move creates with the source bytes.
    pub to: String,
}

/// A commit's record that it is about to write one path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applying {
    /// The path about to be written.
    pub path: String,
    /// Raw hash of the bytes the write leaves.
    pub after: String,
    /// The bytes the write replaces, when they differ from the intent's
    /// `originals[path]` (a concurrent commit landed on the file since
    /// prepare). `None` = the undo target is `originals[path]`.
    #[serde(default, with = "base64_opt", skip_serializing_if = "Option::is_none")]
    pub before: Option<Vec<u8>>,
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

/// Serde adapter for optional bytes as a base64 string.
mod base64_opt {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, ser: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => ser.serialize_str(&STANDARD.encode(b)),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(de)?
            .map(|s| {
                STANDARD
                    .decode(s.as_bytes())
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
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
    /// An intent with an empty id cannot name its file.
    #[error("wal: intent has an empty id")]
    EmptyId,
    /// An intent with this id is already recorded; appending would merge
    /// two ops' records.
    #[error("wal: intent {id:?} is already recorded")]
    DuplicateId { id: String },
    /// A record was appended for an intent that has no file: it was never
    /// recorded, or it was already cleared.
    #[error("wal: no recorded intent {id:?}")]
    UnknownIntent { id: String },
    /// A record carries a format version this reader does not know.
    #[error("wal: {path:?} line {line}: unsupported record version {version}")]
    UnsupportedVersion {
        path: String,
        line: usize,
        version: u64,
    },
    /// A complete JSON record does not have the shape its position calls
    /// for. Not a torn write: the line parsed as JSON.
    #[error("wal: {path:?} line {line}: malformed record: {reason}")]
    Malformed {
        path: String,
        line: usize,
        reason: String,
    },
}

/// The on-wire shape of a versioned record. Exactly one of `applying` and
/// `landed` is set.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    v: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applying: Option<Applying>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    landed: bool,
}

/// Where a replayed intent stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Its first line never became durable: the op stopped inside
    /// [`append`] and changed nothing. Only `intent.id` is known.
    Torn,
    /// Recorded, not landed: the op never finished (or failed and its undo
    /// did not complete). Recovery converges it.
    Pending,
    /// Landed: its bytes are on disk and were reported landed. Recovery
    /// keeps them.
    Landed,
    /// Read from the shared log older versions wrote. It carries no
    /// after-images, so recovery applies the old unconditional rules.
    Legacy,
}

/// One intent read back from the WAL.
#[derive(Debug, Clone, PartialEq)]
pub struct Replayed {
    /// The recorded intent.
    pub intent: Intent,
    /// The commit writes recorded for it, in the order they were made.
    pub applied: Vec<Applying>,
    /// Where it stands.
    pub status: Status,
}

fn io_err(op: &'static str, path: &Path, source: io::Error) -> WalError {
    WalError::Io {
        op,
        path: path.display().to_string(),
        source,
    }
}

/// The file holding intent `id`.
fn intent_file(dir: &Path, id: &str) -> PathBuf {
    let hex: String = id.bytes().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{FILE_PREFIX}{hex}{FILE_SUFFIX}"))
}

/// The intent id a per-intent file name encodes; `None` for any other file.
fn id_of(name: &str) -> Option<Result<String, String>> {
    let hex = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    let decoded = (hex.len() % 2 == 0)
        .then(|| {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
                .collect::<Option<Vec<u8>>>()
        })
        .flatten()
        .and_then(|b| String::from_utf8(b).ok());
    Some(decoded.ok_or_else(|| format!("file name {name:?} does not encode an intent id")))
}

/// Durably records one intent in its own new file. It creates `dir` if
/// needed, writes the intent as one JSON line, and fsyncs the file and the
/// directory before returning, so a caller acting on success (delete unlinks
/// right after) never outruns its undo record. On failure the file may hold
/// a torn line, which replay reads as [`Status::Torn`] and recover removes.
pub fn append(dir: &Path, intent: &Intent) -> Result<(), WalError> {
    if intent.id.is_empty() {
        return Err(WalError::EmptyId);
    }
    let mut line = serde_json::to_vec(intent).map_err(|e| WalError::Marshal {
        id: intent.id.clone(),
        source: e,
    })?;
    line.push(b'\n');

    std::fs::create_dir_all(dir).map_err(|e| io_err("create dir", dir, e))?;
    let path = intent_file(dir, &intent.id);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                WalError::DuplicateId {
                    id: intent.id.clone(),
                }
            } else {
                io_err("create", &path, e)
            }
        })?;
    fault::hit("wal.append").map_err(|e| io_err("write", &path, e))?;
    f.write_all(&line).map_err(|e| io_err("write", &path, e))?;
    f.sync_all().map_err(|e| io_err("fsync", &path, e))?;
    // A fresh file's directory entry is not durable until the directory is
    // synced; without it a power loss can lose the whole record after
    // append returned.
    let d = File::open(dir).map_err(|e| io_err("open dir", dir, e))?;
    d.sync_all().map_err(|e| io_err("sync dir", dir, e))?;
    Ok(())
}

/// Durably records that a commit is about to write `rec.path`. Written
/// before the write, so a crash after it leaves recovery the after-image it
/// needs to tell this commit's bytes from anyone else's.
pub fn record_applying(dir: &Path, id: &str, rec: &Applying) -> Result<(), WalError> {
    fault::hit("wal.applying").map_err(|e| io_err("write", &intent_file(dir, id), e))?;
    append_record(
        dir,
        id,
        &Record {
            v: RECORD_VERSION,
            applying: Some(rec.clone()),
            landed: false,
        },
    )
}

/// Durably records that intent `id`'s bytes are all on disk. Written after
/// the op's last byte lands and before [`clear`], so a failed clear leaves a
/// record recovery reads as "keep", not "undo".
pub fn mark_landed(dir: &Path, id: &str) -> Result<(), WalError> {
    fault::hit("wal.mark").map_err(|e| io_err("write", &intent_file(dir, id), e))?;
    append_record(
        dir,
        id,
        &Record {
            v: RECORD_VERSION,
            applying: None,
            landed: true,
        },
    )
}

/// Appends one versioned record to an EXISTING intent file and fsyncs it.
/// The file must exist: a record for a cleared intent would resurrect it
/// with no intent line.
fn append_record(dir: &Path, id: &str, rec: &Record) -> Result<(), WalError> {
    let mut line = serde_json::to_vec(rec).map_err(|e| WalError::Marshal {
        id: id.to_string(),
        source: e,
    })?;
    line.push(b'\n');
    let path = intent_file(dir, id);
    let mut f = OpenOptions::new().append(true).open(&path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            WalError::UnknownIntent { id: id.to_string() }
        } else {
            io_err("open", &path, e)
        }
    })?;
    f.write_all(&line).map_err(|e| io_err("write", &path, e))?;
    f.sync_all().map_err(|e| io_err("fsync", &path, e))?;
    Ok(())
}

/// Reads every intent in `dir`: the legacy shared log first, then each
/// per-intent file in name order. A missing directory is an empty WAL.
pub fn replay(dir: &Path) -> Result<Vec<Replayed>, WalError> {
    let mut out = replay_legacy(dir)?;
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(io_err("read dir", dir, e)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| io_err("read dir", dir, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = id_of(name) else { continue };
        let path = entry.path();
        let id = id.map_err(|reason| WalError::Malformed {
            path: path.display().to_string(),
            line: 0,
            reason,
        })?;
        files.push((path, id));
    }
    files.sort();
    for (path, id) in files {
        out.push(replay_file(&path, id)?);
    }
    Ok(out)
}

/// The non-empty lines of a WAL file with their 1-based numbers; `None` for
/// a line that does not parse as JSON. Appends are sequential, so only the
/// LAST line can be torn: an unparsable line with anything after it is
/// corruption, not a crash, and fails loud.
fn json_lines(path: &Path) -> Result<Vec<(usize, Option<serde_json::Value>)>, WalError> {
    let f = File::open(path).map_err(|e| io_err("open", path, e))?;
    let mut out: Vec<(usize, Option<serde_json::Value>)> = Vec::new();
    for (idx, line) in BufReader::new(f).split(b'\n').enumerate() {
        let line = line.map_err(|e| io_err("scan", path, e))?;
        if line.is_empty() {
            continue;
        }
        if let Some((torn, None)) = out.last() {
            return Err(malformed(path, *torn, "unparsable line before the end"));
        }
        out.push((idx + 1, serde_json::from_slice(&line).ok()));
    }
    Ok(out)
}

fn malformed(path: &Path, line: usize, reason: impl Into<String>) -> WalError {
    WalError::Malformed {
        path: path.display().to_string(),
        line,
        reason: reason.into(),
    }
}

/// Decodes a complete line as an intent. serde decodes a struct from a JSON
/// array too, so a non-object is rejected first.
fn decode_intent(path: &Path, line: usize, value: serde_json::Value) -> Result<Intent, WalError> {
    if !value.is_object() {
        return Err(malformed(path, line, "record is not a JSON object"));
    }
    if let Some(v) = value.get("v") {
        return Err(match v.as_u64() {
            Some(RECORD_VERSION) => malformed(path, line, "expected an intent record"),
            Some(version) => WalError::UnsupportedVersion {
                path: path.display().to_string(),
                line,
                version,
            },
            None => malformed(
                path,
                line,
                format!("version {v} is not an unsigned integer"),
            ),
        });
    }
    serde_json::from_value::<Intent>(value).map_err(|e| malformed(path, line, e.to_string()))
}

fn decode_record(path: &Path, line: usize, value: serde_json::Value) -> Result<Record, WalError> {
    match value.get("v").map(serde_json::Value::as_u64) {
        None => return Err(malformed(path, line, "a second intent in one intent file")),
        Some(Some(RECORD_VERSION)) => {}
        Some(Some(version)) => {
            return Err(WalError::UnsupportedVersion {
                path: path.display().to_string(),
                line,
                version,
            });
        }
        Some(None) => return Err(malformed(path, line, "version is not an unsigned integer")),
    }
    let rec = serde_json::from_value::<Record>(value)
        .map_err(|e| malformed(path, line, e.to_string()))?;
    if rec.landed == rec.applying.is_some() {
        return Err(malformed(
            path,
            line,
            "a record sets exactly one of `applying` and `landed`",
        ));
    }
    Ok(rec)
}

/// Intents from the shared log older versions wrote. Only intent lines were
/// ever written there.
fn replay_legacy(dir: &Path) -> Result<Vec<Replayed>, WalError> {
    let path = dir.join(LEGACY_LOG);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (line, value) in json_lines(&path)? {
        let Some(value) = value else { break };
        out.push(Replayed {
            intent: decode_intent(&path, line, value)?,
            applied: Vec::new(),
            status: Status::Legacy,
        });
    }
    Ok(out)
}

fn replay_file(path: &Path, id: String) -> Result<Replayed, WalError> {
    let mut lines = json_lines(path)?.into_iter();
    let Some((line, Some(first))) = lines.next() else {
        return Ok(Replayed {
            intent: Intent {
                id,
                ..Default::default()
            },
            applied: Vec::new(),
            status: Status::Torn,
        });
    };
    let intent = decode_intent(path, line, first)?;
    if intent.id != id {
        return Err(malformed(
            path,
            line,
            format!("intent id {:?} does not match its file name", intent.id),
        ));
    }
    let mut applied = Vec::new();
    let mut status = Status::Pending;
    for (line, value) in lines {
        let Some(value) = value else { break };
        let rec = decode_record(path, line, value)?;
        if status == Status::Landed {
            return Err(malformed(path, line, "a record after the landed marker"));
        }
        match rec.applying {
            Some(a) => applied.push(a),
            None => status = Status::Landed,
        }
    }
    Ok(Replayed {
        intent,
        applied,
        status,
    })
}

/// Removes intent `id`'s file, if present, leaving every other intent in
/// `dir` untouched.
pub fn clear(dir: &Path, id: &str) -> Result<(), WalError> {
    let path = intent_file(dir, id);
    fault::hit("wal.clear").map_err(|e| io_err("remove", &path, e))?;
    remove(&path)
}

/// Removes the legacy shared log, if present. Only recovery, which has
/// converged every intent in it, calls this.
pub fn clear_legacy(dir: &Path) -> Result<(), WalError> {
    remove(&dir.join(LEGACY_LOG))
}

fn remove(path: &Path) -> Result<(), WalError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove", path, e)),
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

    fn pending(intent: Intent) -> Replayed {
        Replayed {
            intent,
            applied: Vec::new(),
            status: Status::Pending,
        }
    }

    fn raw_append(path: &Path, bytes: &[u8]) {
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
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
        b.after = AfterImages::from([("new.txt".into(), Some("h".into()))]);
        append(dir.path(), &a).unwrap();
        append(dir.path(), &b).unwrap();
        assert_eq!(replay(dir.path()).unwrap(), vec![pending(a), pending(b)]);
    }

    #[test]
    fn clearing_one_intent_keeps_every_other() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["i1", "i2", "i3"] {
            append(dir.path(), &sample(id)).unwrap();
        }
        clear(dir.path(), "i2").unwrap();
        let ids: Vec<String> = replay(dir.path())
            .unwrap()
            .into_iter()
            .map(|r| r.intent.id)
            .collect();
        assert_eq!(ids, vec!["i1".to_string(), "i3".to_string()]);
    }

    #[test]
    fn applying_and_landed_records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("i1")).unwrap();
        let a1 = Applying {
            path: "a.txt".into(),
            after: "h1".into(),
            before: None,
        };
        let a2 = Applying {
            path: "b.txt".into(),
            after: "h2".into(),
            before: Some(b"was\xff".to_vec()),
        };
        record_applying(dir.path(), "i1", &a1).unwrap();
        record_applying(dir.path(), "i1", &a2).unwrap();
        mark_landed(dir.path(), "i1").unwrap();
        let got = replay(dir.path()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].applied, vec![a1, a2]);
        assert_eq!(got[0].status, Status::Landed);
        let text = std::fs::read_to_string(intent_file(dir.path(), "i1")).unwrap();
        assert!(
            text.ends_with(
                "{\"v\":2,\"applying\":{\"path\":\"b.txt\",\"after\":\"h2\",\"before\":\"d2Fz/w==\"}}\n{\"v\":2,\"landed\":true}\n"
            ),
            "wire shape: {text}"
        );
    }

    #[test]
    fn records_for_an_unrecorded_intent_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = mark_landed(dir.path(), "ghost").unwrap_err();
        assert!(matches!(err, WalError::UnknownIntent { .. }), "{err}");
        assert!(replay(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn duplicate_and_empty_ids_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("dup")).unwrap();
        let err = append(dir.path(), &sample("dup")).unwrap_err();
        assert!(matches!(err, WalError::DuplicateId { .. }), "{err}");
        let err = append(dir.path(), &sample("")).unwrap_err();
        assert!(matches!(err, WalError::EmptyId), "{err}");
    }

    #[test]
    fn torn_marker_reads_as_not_landed() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("i1")).unwrap();
        raw_append(&intent_file(dir.path(), "i1"), b"{\"v\":2,\"land");
        let got = replay(dir.path()).unwrap();
        assert_eq!(got[0].status, Status::Pending);
    }

    #[test]
    fn an_unparsable_line_before_the_end_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("i1")).unwrap();
        raw_append(
            &intent_file(dir.path(), "i1"),
            b"{\"v\":2,\"land\n{\"v\":2,\"landed\":true}\n",
        );
        let err = replay(dir.path()).unwrap_err();
        assert!(matches!(err, WalError::Malformed { line: 2, .. }), "{err}");
    }

    #[test]
    fn torn_intent_line_reads_as_torn() {
        let dir = tempfile::tempdir().unwrap();
        raw_append(&intent_file(dir.path(), "t1"), b"{\"id\":\"t1\",\"orig");
        let got = replay(dir.path()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].status, Status::Torn);
        assert_eq!(got[0].intent.id, "t1");
    }

    #[test]
    fn a_record_after_the_landed_marker_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("i1")).unwrap();
        mark_landed(dir.path(), "i1").unwrap();
        mark_landed(dir.path(), "i1").unwrap();
        let err = replay(dir.path()).unwrap_err();
        assert!(matches!(err, WalError::Malformed { line: 3, .. }), "{err}");
    }

    #[test]
    fn unknown_version_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("i1")).unwrap();
        raw_append(
            &intent_file(dir.path(), "i1"),
            b"{\"v\":3,\"landed\":true}\n",
        );
        let err = replay(dir.path()).unwrap_err();
        assert!(
            matches!(
                err,
                WalError::UnsupportedVersion {
                    version: 3,
                    line: 2,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn complete_but_misshapen_records_fail_loud() {
        for bad in [
            &b"{\"v\":2,\"landed\":true,\"extra\":1}\n"[..],
            b"{\"v\":2}\n",
            b"{\"v\":2,\"landed\":true,\"applying\":{\"path\":\"p\",\"after\":\"h\"}}\n",
            b"{\"v\":2,\"applying\":{\"path\":\"p\"}}\n",
            b"{\"v\":\"2\",\"landed\":true}\n",
            b"{\"id\":\"i1\"}\n",
            b"[]\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            append(dir.path(), &sample("i1")).unwrap();
            raw_append(&intent_file(dir.path(), "i1"), bad);
            let err = replay(dir.path()).unwrap_err();
            assert!(
                matches!(err, WalError::Malformed { line: 2, .. }),
                "{} -> {err}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn a_file_whose_name_disagrees_with_its_intent_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        let line = serde_json::to_vec(&sample("other")).unwrap();
        raw_append(&intent_file(dir.path(), "i1"), &line);
        let err = replay(dir.path()).unwrap_err();
        assert!(matches!(err, WalError::Malformed { line: 1, .. }), "{err}");

        let dir = tempfile::tempdir().unwrap();
        raw_append(&dir.path().join("bage-intent-zz.wal"), b"{}\n");
        let err = replay(dir.path()).unwrap_err();
        assert!(matches!(err, WalError::Malformed { .. }), "{err}");
    }

    #[test]
    fn unrelated_files_in_the_dir_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        raw_append(&dir.path().join("someone-else.wal"), b"not ours");
        append(dir.path(), &sample("i1")).unwrap();
        assert_eq!(replay(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn legacy_shared_log_replays_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(LEGACY_LOG);
        raw_append(
            &legacy,
            &[serde_json::to_vec(&sample("old")).unwrap(), b"\n".to_vec()].concat(),
        );
        raw_append(&legacy, b"{\"id\":\"torn");
        append(dir.path(), &sample("new")).unwrap();
        let got = replay(dir.path()).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            (got[0].intent.id.as_str(), got[0].status),
            ("old", Status::Legacy)
        );
        assert_eq!(got[1].status, Status::Pending);
        clear_legacy(dir.path()).unwrap();
        assert_eq!(replay(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn replay_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(replay(dir.path()).unwrap().is_empty());
        assert!(replay(&dir.path().join("nonexistent")).unwrap().is_empty());
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
        assert!(j.get("after").is_none());
        assert_eq!(j["edits"][0]["StartByte"], 0);
        // And a Go-written record (absent optional keys) decodes cleanly.
        let go_record = r#"{"id":"g","edits":null,"originals":{"p":"aGk="},"expected_raw_hash":{},"expected_norm_hash":{}}"#;
        let intent: Intent = serde_json::from_str(go_record).unwrap();
        assert_eq!(intent.id, "g");
        assert_eq!(intent.originals["p"], b"hi");
        assert!(!intent.batch);
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample("i1")).unwrap();
        clear(dir.path(), "i1").unwrap();
        assert!(replay(dir.path()).unwrap().is_empty());
        clear(dir.path(), "i1").unwrap();
    }
}
