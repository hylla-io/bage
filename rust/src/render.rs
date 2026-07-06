//! Output formats for bage command results: text, json, and toon.
//!
//! Port of Go `pkg/render`. [`Format`] identifies how a command renders its
//! output, [`emit`] writes a value to a writer in a chosen format, and
//! [`marshal_toon`] is a hand-ported subset of the toon-go encoder that
//! byte-matches the Go binary's `--format toon` output for Båge's result
//! shapes (uniform tabular arrays, plain objects, scalars, quoted strings).

use std::io::{self, Write};
use std::str::FromStr;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

/// Format identifies how a command renders its output. The default (an empty
/// `--format` flag) is [`Format::Text`]; parse flag values with `FromStr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Human-readable plain text (the default).
    Text,
    /// Machine-readable JSON, indented with two spaces plus a trailing
    /// newline (byte-identical with Go's `json.MarshalIndent(v, "", "  ")`
    /// for shapes without HTML-escaped characters).
    Json,
    /// TOON (token-oriented object notation).
    Toon,
}

impl FromStr for Format {
    type Err = RenderError;

    /// Maps a `--format` flag value to a Format. An EMPTY string resolves to
    /// [`Format::Text`], the default; anything other than
    /// `"text" | "json" | "toon"` is an explicit usage error rather than a
    /// silent fallthrough (mirrors Go `ParseFormat`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "" | "text" => Ok(Format::Text),
            "json" => Ok(Format::Json),
            "toon" => Ok(Format::Toon),
            other => Err(RenderError::UnknownFormat(other.to_string())),
        }
    }
}

/// Errors surfaced by the render layer.
#[derive(Debug, Error)]
pub enum RenderError {
    /// The `--format` flag value is not a known format.
    #[error("bage: unknown --format {0:?} (want text|json|toon)")]
    UnknownFormat(String),
    /// JSON serialization failed.
    #[error("render: marshal json: {0}")]
    Json(#[from] serde_json::Error),
    /// TOON encoding rejected the value (e.g. an unsupported control
    /// character inside a string).
    #[error("render: marshal toon: {0}")]
    Toon(String),
    /// Writing to the output failed.
    #[error("render: write: {0}")]
    Io(#[from] io::Error),
}

/// TextRender is implemented by result types that know how to render
/// themselves as human-readable text — the Rust shape of Go's
/// `render.TextRenderable` interface. Where Go type-asserts at runtime, Rust
/// requires the bound at compile time, so a non-renderable value is a compile
/// error rather than Go's runtime error.
pub trait TextRender {
    /// Writes the receiver's human-readable representation to `w`.
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()>;
}

/// Writes `v` to `w` in the given [`Format`]: JSON as two-space-indented
/// `serde_json` output plus a trailing newline (matching Go's
/// `json.MarshalIndent(v, "", "  ")` + `Fprintln`), text via the value's
/// [`TextRender`] impl, and TOON via [`marshal_toon`] (no trailing newline,
/// matching the Go binary).
pub fn emit<T: Serialize + TextRender>(
    w: &mut dyn Write,
    f: Format,
    v: &T,
) -> Result<(), RenderError> {
    match f {
        Format::Json => {
            serde_json::to_writer_pretty(&mut *w, v)?;
            w.write_all(b"\n")?;
            Ok(())
        }
        Format::Text => Ok(v.render_text(w)?),
        Format::Toon => {
            let value = serde_json::to_value(v)?;
            let doc = marshal_toon(&value)?;
            w.write_all(doc.as_bytes())?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// TOON encoder — hand-ported subset of toon-go (comma delimiter, two-space
// indent, no length markers: the exact options Go MarshalTOON pins).
// ---------------------------------------------------------------------------

/// JavaScript's `Number.MAX_SAFE_INTEGER`: integers beyond it lose IEEE 754
/// precision, so toon-go renders them as strings. Mirrored here.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// A value normalized to the TOON data model (toon-go `normalizedValue`).
/// Numbers carry their literal rendering; objects preserve key order.
enum Norm {
    Null,
    Bool(bool),
    Str(String),
    Num(String),
    Arr(Vec<Norm>),
    Obj(Vec<(String, Norm)>),
}

impl Norm {
    /// Reports whether the value is a TOON primitive (null/bool/string/number).
    fn is_primitive(&self) -> bool {
        matches!(
            self,
            Norm::Null | Norm::Bool(_) | Norm::Str(_) | Norm::Num(_)
        )
    }
}

/// Encodes `v` as a TOON document with the options the Go binary uses (comma
/// array delimiter, two-space indent, no length markers). The output carries
/// NO trailing newline, byte-matching Go `render.MarshalTOON`. Field order
/// follows the `serde_json::Value` map order (this crate enables
/// `preserve_order`, so struct field declaration order survives `to_value`,
/// matching Go's struct-field normalization).
pub fn marshal_toon(v: &Value) -> Result<String, RenderError> {
    let norm = normalize(v);
    let mut lines: Vec<String> = Vec::new();
    encode_root(&norm, &mut lines)?;
    Ok(lines.join("\n"))
}

/// Maps a `serde_json::Value` into the TOON data model. Integers beyond the
/// safe-integer range become strings; `-0.0` normalizes to `0`; floats render
/// via Rust's shortest round-trip `Display`, matching Go's
/// `strconv.FormatFloat(f, 'f', -1, 64)` for the values Båge emits.
fn normalize(v: &Value) -> Norm {
    match v {
        Value::Null => Norm::Null,
        Value::Bool(b) => Norm::Bool(*b),
        Value::String(s) => Norm::Str(s.clone()),
        Value::Number(n) => normalize_number(n),
        Value::Array(items) => Norm::Arr(items.iter().map(normalize).collect()),
        Value::Object(map) => Norm::Obj(
            map.iter()
                .map(|(k, val)| (k.clone(), normalize(val)))
                .collect(),
        ),
    }
}

/// Normalizes one JSON number per toon-go's rules (see [`normalize`]).
fn normalize_number(n: &serde_json::Number) -> Norm {
    if let Some(i) = n.as_i64() {
        if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&i) {
            return Norm::Str(i.to_string());
        }
        return Norm::Num(i.to_string());
    }
    if let Some(u) = n.as_u64() {
        if u > MAX_SAFE_INTEGER as u64 {
            return Norm::Str(u.to_string());
        }
        return Norm::Num(u.to_string());
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f == 0.0 {
        // Covers -0.0, which toon-go folds to plain 0.
        return Norm::Num("0".to_string());
    }
    Norm::Num(format!("{f}"))
}

/// Encodes the document root (toon-go `encodeState.encodeRoot`). An empty
/// object at the root emits nothing.
fn encode_root(v: &Norm, lines: &mut Vec<String>) -> Result<(), RenderError> {
    match v {
        Norm::Obj(fields) => {
            if fields.is_empty() {
                return Ok(());
            }
            encode_object(fields, 0, lines)
        }
        Norm::Arr(items) => encode_array("", items, 0, lines),
        prim => {
            lines.push(format_primitive(prim)?);
            Ok(())
        }
    }
}

/// Renders `depth` levels of two-space indentation.
fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

/// Encodes an object's fields at `depth` (toon-go `encodeObject`).
fn encode_object(
    fields: &[(String, Norm)],
    depth: usize,
    lines: &mut Vec<String>,
) -> Result<(), RenderError> {
    let ind = indent(depth);
    for (key, value) in fields {
        match value {
            Norm::Obj(children) => {
                lines.push(format!("{ind}{}:", encode_key(key)?));
                encode_object(children, depth + 1, lines)?;
            }
            Norm::Arr(items) => encode_array(&encode_key(key)?, items, depth, lines)?,
            prim => {
                lines.push(format!(
                    "{ind}{}: {}",
                    encode_key(key)?,
                    format_primitive(prim)?
                ));
            }
        }
    }
    Ok(())
}

/// Encodes an array at `depth` (toon-go `encodeArray`): a primitive array
/// inlines after its `key[N]:` header, a uniform array of flat objects
/// renders as a tabular block with a `{field,…}` header, and anything else
/// falls back to `- ` list items.
fn encode_array(
    key_literal: &str,
    values: &[Norm],
    depth: usize,
    lines: &mut Vec<String>,
) -> Result<(), RenderError> {
    let ind = indent(depth);

    if values.iter().all(Norm::is_primitive) {
        let mut line = format!("{ind}{}", render_header(key_literal, values.len(), None)?);
        if !values.is_empty() {
            let tokens: Result<Vec<String>, RenderError> =
                values.iter().map(format_primitive).collect();
            line.push(' ');
            line.push_str(&tokens?.join(","));
        }
        lines.push(line);
        return Ok(());
    }

    if let Some(fields) = detect_tabular(values) {
        lines.push(format!(
            "{ind}{}",
            render_header(key_literal, values.len(), Some(&fields))?
        ));
        emit_tabular_rows(values, &fields, depth + 1, lines)?;
        return Ok(());
    }

    lines.push(format!(
        "{ind}{}",
        render_header(key_literal, values.len(), None)?
    ));
    for item in values {
        encode_list_item(item, depth + 1, lines)?;
    }
    Ok(())
}

/// Emits one comma-joined row line per tabular object at `depth`.
fn emit_tabular_rows(
    values: &[Norm],
    fields: &[String],
    depth: usize,
    lines: &mut Vec<String>,
) -> Result<(), RenderError> {
    for row in values {
        let Norm::Obj(row_fields) = row else {
            unreachable!("detect_tabular guarantees objects")
        };
        let mut tokens = Vec::with_capacity(fields.len());
        for field in fields {
            tokens.push(format_primitive(obj_field(row_fields, field))?);
        }
        lines.push(format!("{}{}", indent(depth), tokens.join(",")));
    }
    Ok(())
}

/// Encodes one `- ` list item (toon-go `encodeListItem`/`encodeArrayItem`,
/// whose bodies are identical).
fn encode_list_item(item: &Norm, depth: usize, lines: &mut Vec<String>) -> Result<(), RenderError> {
    match item {
        Norm::Obj(fields) => encode_object_list_item(fields, depth, lines),
        Norm::Arr(items) => encode_array_for_object_list_item("", items, depth, lines),
        prim => {
            lines.push(format!("{}- {}", indent(depth), format_primitive(prim)?));
            Ok(())
        }
    }
}

/// Encodes an object appearing as a list item (toon-go
/// `encodeObjectListItem`): the first field rides the `- ` line, the rest
/// nest one level deeper.
fn encode_object_list_item(
    fields: &[(String, Norm)],
    depth: usize,
    lines: &mut Vec<String>,
) -> Result<(), RenderError> {
    let ind = indent(depth);
    let Some((first_key, first_value)) = fields.first() else {
        lines.push(format!("{ind}- {{}}"));
        return Ok(());
    };
    if first_value.is_primitive() {
        lines.push(format!(
            "{ind}- {}: {}",
            encode_key(first_key)?,
            format_primitive(first_value)?
        ));
        if fields.len() > 1 {
            encode_object(&fields[1..], depth + 1, lines)?;
        }
        return Ok(());
    }
    if let Norm::Arr(items) = first_value {
        encode_array_for_object_list_item(&encode_key(first_key)?, items, depth, lines)?;
        if fields.len() > 1 {
            encode_object(&fields[1..], depth + 1, lines)?;
        }
        return Ok(());
    }
    lines.push(format!("{ind}-"));
    encode_object(fields, depth + 1, lines)
}

/// Encodes an array riding a `- ` list-item line (toon-go
/// `encodeArrayForObjectListItem`). Note the check order differs from
/// [`encode_array`]: tabular is preferred over primitive-inline, mirroring Go.
fn encode_array_for_object_list_item(
    key_literal: &str,
    values: &[Norm],
    depth: usize,
    lines: &mut Vec<String>,
) -> Result<(), RenderError> {
    let ind = indent(depth);

    if let Some(fields) = detect_tabular(values) {
        lines.push(format!(
            "{ind}- {}",
            render_header(key_literal, values.len(), Some(&fields))?
        ));
        emit_tabular_rows(values, &fields, depth + 1, lines)?;
        return Ok(());
    }

    if values.iter().all(Norm::is_primitive) {
        let mut line = format!("{ind}- {}", render_header(key_literal, values.len(), None)?);
        if !values.is_empty() {
            let tokens: Result<Vec<String>, RenderError> =
                values.iter().map(format_primitive).collect();
            line.push(' ');
            line.push_str(&tokens?.join(","));
        }
        lines.push(line);
        return Ok(());
    }

    lines.push(format!(
        "{ind}- {}",
        render_header(key_literal, values.len(), None)?
    ));
    for item in values {
        encode_list_item(item, depth + 1, lines)?;
    }
    Ok(())
}

/// Reports whether `values` is a uniform array of flat objects — same key
/// set, all primitive values — returning the header field order (the first
/// object's key order) when it is. Port of toon-go `detectTabular`.
fn detect_tabular(values: &[Norm]) -> Option<Vec<String>> {
    let Norm::Obj(first) = values.first()? else {
        return None;
    };
    if first.is_empty() {
        return None;
    }
    let mut fields = Vec::with_capacity(first.len());
    for (key, value) in first {
        if !value.is_primitive() {
            return None;
        }
        fields.push(key.clone());
    }
    for value in &values[1..] {
        let Norm::Obj(obj) = value else {
            return None;
        };
        if obj.len() != fields.len() {
            return None;
        }
        for (key, field_value) in obj {
            if !fields.contains(key) || !field_value.is_primitive() {
                return None;
            }
        }
    }
    Some(fields)
}

/// Looks up a field by key within a normalized object's fields.
fn obj_field<'a>(fields: &'a [(String, Norm)], key: &str) -> &'a Norm {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or(&Norm::Null)
}

/// Renders an array header `key[N]:` or tabular `key[N]{f1,f2}:` (toon-go
/// `renderHeader` with the comma delimiter, which is omitted from brackets,
/// and no length markers).
fn render_header(
    key_literal: &str,
    length: usize,
    fields: Option<&[String]>,
) -> Result<String, RenderError> {
    let mut out = String::new();
    out.push_str(key_literal);
    out.push('[');
    out.push_str(&length.to_string());
    out.push(']');
    if let Some(fields) = fields {
        out.push('{');
        for (i, field) in fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&encode_key(field)?);
        }
        out.push('}');
    }
    out.push(':');
    Ok(out)
}

/// Renders a primitive token (toon-go `formatPrimitive`).
fn format_primitive(v: &Norm) -> Result<String, RenderError> {
    match v {
        Norm::Null => Ok("null".to_string()),
        Norm::Bool(true) => Ok("true".to_string()),
        Norm::Bool(false) => Ok("false".to_string()),
        Norm::Num(literal) => Ok(literal.clone()),
        Norm::Str(s) => format_string(s),
        _ => Err(RenderError::Toon("non-primitive token".to_string())),
    }
}

/// Applies TOON string quoting: validates control characters, then quotes
/// only when the concrete syntax requires it (toon-go `FormatString`).
fn format_string(s: &str) -> Result<String, RenderError> {
    validate_characters(s)?;
    if needs_quoting(s) {
        return quote_string(s);
    }
    Ok(s.to_string())
}

/// Reports whether `s` must be double-quoted (toon-go `NeedsQuoting`). Both
/// the array and document delimiters are the comma here, so the two
/// delimiter-containment rules collapse into one.
fn needs_quoting(s: &str) -> bool {
    if s.is_empty() || s.trim() != s {
        return true;
    }
    if matches!(s, "true" | "false" | "null") {
        return true;
    }
    if looks_numeric(s) || has_leading_zero_decimal(s) {
        return true;
    }
    if s.contains([':', '\\', '"', '[', ']', '{', '}', '\n', '\r', '\t', ',']) {
        return true;
    }
    s.starts_with('-')
}

/// Escapes and double-quotes `s` (toon-go `QuoteString`).
fn quote_string(s: &str) -> Result<String, RenderError> {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                return Err(RenderError::Toon(format!(
                    "unsupported control character U+{:04X} in string",
                    c as u32
                )));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

/// Rejects control characters TOON cannot carry (toon-go
/// `ValidateCharacters`).
fn validate_characters(s: &str) -> Result<(), RenderError> {
    for c in s.chars() {
        if (c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t') {
            return Err(RenderError::Toon(format!(
                "unsupported control character U+{:04X} in string",
                c as u32
            )));
        }
    }
    Ok(())
}

/// Reports whether `s` resembles a numeric literal — such strings must be
/// quoted so decoding does not misread them (toon-go `LooksNumeric`).
fn looks_numeric(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;
    if b[0] == b'-' {
        i += 1;
        if i == b.len() {
            return false;
        }
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        if i == b.len() || !b[i].is_ascii_digit() {
            return false;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        if i == b.len() || !b[i].is_ascii_digit() {
            return false;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    i == b.len()
}

/// Reports whether `s` starts with a forbidden leading-zero digit pair — e.g.
/// the region_hash "04927316d00f5017", which the Go binary quotes (toon-go
/// `HasLeadingZeroDecimal`).
fn has_leading_zero_decimal(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0] == b'0' && b[1].is_ascii_digit()
}

/// Applies TOON key quoting (toon-go `EncodeKey`): identifier-shaped keys
/// stay bare, everything else is quoted.
fn encode_key(key: &str) -> Result<String, RenderError> {
    if is_valid_unquoted_key(key) {
        return Ok(key.to_string());
    }
    quote_string(key)
}

/// Reports whether `key` matches the bare-identifier pattern (toon-go
/// `IsValidUnquotedKey`): a letter or `_` first, then letters, digits, `_`,
/// or `.`.
fn is_valid_unquoted_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '_' && !first.is_alphabetic() {
        return false;
    }
    chars.all(|c| c.is_alphabetic() || c.is_numeric() || c == '_' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use serde_json::json;

    // ---- Format parsing (Go format_test.go TestParseFormat) ----

    #[test]
    fn parse_format() {
        assert_eq!("text".parse::<Format>().unwrap(), Format::Text);
        assert_eq!("json".parse::<Format>().unwrap(), Format::Json);
        assert_eq!("toon".parse::<Format>().unwrap(), Format::Toon);
        // Empty defaults to text.
        assert_eq!("".parse::<Format>().unwrap(), Format::Text);
        // Unknown errors, and the message names every valid format.
        let err = "xml".parse::<Format>().unwrap_err();
        let msg = err.to_string();
        for valid in ["text", "json", "toon"] {
            assert!(msg.contains(valid), "error {msg:?} does not name {valid:?}");
        }
    }

    // ---- Emit (Go render_test.go) ----

    #[derive(Serialize)]
    struct Payload {
        name: String,
        count: i32,
    }

    impl TextRender for Payload {
        fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
            write!(w, "X")
        }
    }

    #[test]
    fn emit_json_two_space_indent_trailing_newline() {
        let v = Payload {
            name: "alpha".to_string(),
            count: 3,
        };
        let mut buf = Vec::new();
        emit(&mut buf, Format::Json, &v).unwrap();
        let want = "{\n  \"name\": \"alpha\",\n  \"count\": 3\n}\n";
        assert_eq!(String::from_utf8(buf).unwrap(), want);
    }

    #[test]
    fn emit_text_delegates_to_render_text() {
        let v = Payload {
            name: "alpha".to_string(),
            count: 3,
        };
        let mut buf = Vec::new();
        emit(&mut buf, Format::Text, &v).unwrap();
        assert_eq!(buf, b"X");
    }

    // NOTE: Go's TestEmitTextNonRenderable (runtime error for a value that
    // does not implement TextRenderable) has no Rust equivalent — the
    // `T: TextRender` bound makes it a compile error instead.

    #[test]
    fn emit_toon_tabular_rows() {
        #[derive(Serialize)]
        struct Row {
            name: &'static str,
            age: i32,
        }
        impl TextRender for Row {
            fn render_text(&self, _w: &mut dyn Write) -> io::Result<()> {
                Ok(())
            }
        }
        let rows = [
            Row {
                name: "alice",
                age: 30,
            },
            Row {
                name: "bob",
                age: 25,
            },
        ];
        let value = serde_json::to_value(rows).unwrap();
        let out = marshal_toon(&value).unwrap();
        assert_eq!(out, "[2]{name,age}:\n  alice,30\n  bob,25");
    }

    // ---- TOON goldens captured from the Go binary (./bin/bage --format
    // toon vs --format json for the same command). Each test parses the
    // captured JSON into a Value (preserve_order keeps the Go struct field
    // order) and asserts marshal_toon reproduces the captured TOON bytes.

    fn golden_roundtrip(json_src: &str, want_toon: &str) {
        let value: Value = serde_json::from_str(json_src).unwrap();
        let got = marshal_toon(&value).unwrap();
        assert_eq!(got, want_toon.strip_suffix('\n').unwrap_or(want_toon));
    }

    #[test]
    fn golden_show_view() {
        golden_roundtrip(
            include_str!("../testdata/toon/show.json"),
            include_str!("../testdata/toon/show.toon"),
        );
    }

    #[test]
    fn golden_read_view() {
        golden_roundtrip(
            include_str!("../testdata/toon/read.json"),
            include_str!("../testdata/toon/read.toon"),
        );
    }

    #[test]
    fn golden_create_edit_results() {
        golden_roundtrip(
            include_str!("../testdata/toon/create.json"),
            include_str!("../testdata/toon/create.toon"),
        );
    }

    #[test]
    fn golden_json_emit_matches_go_marshal_indent() {
        // The captured create.json came from Go json.MarshalIndent(v,"","  ")
        // + newline; emitting the parsed Value must reproduce it byte-for-byte.
        #[derive(Serialize)]
        struct Wrap(Value);
        impl TextRender for Wrap {
            fn render_text(&self, _w: &mut dyn Write) -> io::Result<()> {
                Ok(())
            }
        }
        for golden in [
            include_str!("../testdata/toon/show.json"),
            include_str!("../testdata/toon/read.json"),
            include_str!("../testdata/toon/create.json"),
        ] {
            let value: Value = serde_json::from_str(golden).unwrap();
            let mut buf = Vec::new();
            emit(&mut buf, Format::Json, &Wrap(value)).unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), golden);
        }
    }

    // ---- Error envelope shape. NOTE: the Go binary currently prints
    // NOTHING for `--format toon` error envelopes — session.Kind is a named
    // string type toon-go's normalize rejects (no reflect.String case), and
    // cmd/bage discards the Emit error. The Rust encoder works over
    // serde_json::Value, where Kind is a plain string, so the envelope
    // renders; this pins the intended shape rather than the Go bug.

    #[test]
    fn error_envelope_toon_shape() {
        let env = json!({
            "kind": "exists",
            "message": "session: create \"/tmp/x\": session: target already exists",
        });
        let got = marshal_toon(&env).unwrap();
        assert_eq!(
            got,
            "kind: exists\nmessage: \"session: create \\\"/tmp/x\\\": session: target already exists\""
        );
    }

    // ---- TOON encoder unit coverage ----

    #[test]
    fn toon_scalar_roots() {
        assert_eq!(marshal_toon(&json!(42)).unwrap(), "42");
        assert_eq!(marshal_toon(&json!(true)).unwrap(), "true");
        assert_eq!(marshal_toon(&json!(null)).unwrap(), "null");
        assert_eq!(marshal_toon(&json!("plain")).unwrap(), "plain");
        assert_eq!(marshal_toon(&json!(1.5)).unwrap(), "1.5");
        assert_eq!(marshal_toon(&json!({})).unwrap(), "");
    }

    #[test]
    fn toon_string_quoting_rules() {
        // Strings that must be quoted.
        for (input, want) in [
            ("", "\"\""),
            ("true", "\"true\""),
            ("42", "\"42\""),
            ("-x", "\"-x\""),
            (" pad", "\" pad\""),
            ("a,b", "\"a,b\""),
            ("a:b", "\"a:b\""),
            ("04927316d00f5017", "\"04927316d00f5017\""),
            ("line1\nline2", "\"line1\\nline2\""),
            ("tab\there", "\"tab\\there\""),
            ("q\"q", "\"q\\\"q\""),
            ("back\\slash", "\"back\\\\slash\""),
        ] {
            assert_eq!(
                marshal_toon(&json!(input)).unwrap(),
                want,
                "input {input:?}"
            );
        }
        // Strings that stay bare.
        for input in ["hello", "9c92adf0bf575068", "tiny.go", "a/b/c.txt", "1.2.3"] {
            assert_eq!(
                marshal_toon(&json!(input)).unwrap(),
                input,
                "input {input:?}"
            );
        }
        // Unsupported control characters error rather than emit garbage.
        assert!(marshal_toon(&json!("bell\u{0007}")).is_err());
    }

    #[test]
    fn toon_nested_object_and_primitive_array() {
        let v = json!({
            "name": "demo",
            "meta": {"lang": "go", "lines": 12},
            "tags": ["a", "b", "c"],
            "empty": [],
        });
        let want = "name: demo\nmeta:\n  lang: go\n  lines: 12\ntags[3]: a,b,c\nempty[0]:";
        assert_eq!(marshal_toon(&v).unwrap(), want);
    }

    #[test]
    fn toon_mixed_array_falls_back_to_list_items() {
        let v = json!(["x", {"a": 1, "b": {"c": 2}}, [1, 2]]);
        let want = "[3]:\n  - x\n  - a: 1\n    b:\n      c: 2\n  - [2]: 1,2";
        assert_eq!(marshal_toon(&v).unwrap(), want);
    }

    #[test]
    fn toon_non_uniform_object_array_uses_list_form() {
        // Differing key sets defeat tabular detection.
        let v = json!([{"a": 1}, {"b": 2}]);
        let want = "[2]:\n  - a: 1\n  - b: 2";
        assert_eq!(marshal_toon(&v).unwrap(), want);
    }

    #[test]
    fn toon_number_edges() {
        // Beyond MAX_SAFE_INTEGER integers render as strings (bare here since
        // looks_numeric strings get quoted — matching toon-go, which returns
        // the decimal string and then quotes it).
        assert_eq!(
            marshal_toon(&json!(9007199254740992u64)).unwrap(),
            "\"9007199254740992\""
        );
        assert_eq!(marshal_toon(&json!(-0.0)).unwrap(), "0");
        assert_eq!(
            marshal_toon(&json!(9007199254740991u64)).unwrap(),
            "9007199254740991"
        );
    }

    #[test]
    fn toon_quoted_key() {
        let v = json!({"weird key": 1, "ok_key.sub": 2});
        assert_eq!(marshal_toon(&v).unwrap(), "\"weird key\": 1\nok_key.sub: 2");
    }
}
