//! Tier-2 analysis substrate: opt-in, best-effort classification of
//! control-flow and resource *sites* over a parsed tree — allocations,
//! resource acquire/release, lock acquire/release, early returns, panics,
//! defers, and spawns.
//!
//! WHY it lives beside — not inside — the outline: the outline
//! ([`crate::inspect::read_blocks`]) is the STABLE declaration floor Hylla
//! anchors nodes on and must stay byte-identical (regression-locked). Tier-2
//! sites are a DISPOSABLE analysis layer Hylla's tier-2 graph consumes to
//! reason about resource lifetimes ("opened here, never released before this
//! return"). Keeping them in their own surface preserves the declaration
//! contract while adding the substrate as a pure opt-in.
//!
//! Classification is grammar-kind + call-target based and LANGUAGE-SCOPED:
//! only Rust, Go, Python, and TS/TSX/JS carry tables (the first-drop set);
//! every other [`Lang`] returns an empty vec — a valid opt-out, never an
//! error. Sites deliberately have NO stable identity: they are re-derived on
//! every parse and never anchored across edits, so overlap is expected (a
//! `defer f.Close()` is BOTH a [`Tier2Family::Defer`] and a
//! [`Tier2Family::ResourceRelease`] site).

use serde::{Deserialize, Serialize};

use crate::inspect::OpenedFile;
use crate::parser::{Lang, Node};
use crate::region;

/// The kind of analysis-substrate signal a [`Site`] carries. The set is the
/// tier-2 family taxonomy (DL-45); a language table maps grammar kinds and
/// call targets onto it. Not every family occurs in every language (e.g. Rust
/// has no explicit `Defer`), and absence is silent, not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier2Family {
    /// Heap/collection allocation (`Box::new`, Go `make`/`new`, JS `new`).
    Allocation,
    /// A resource is opened/acquired (`File::open`, `os.Open`, `open(...)`).
    ResourceAcquisition,
    /// A resource is closed/released (`drop`, `.Close()`, `.close()`).
    ResourceRelease,
    /// An explicit `return` control-flow exit (every return, since any return
    /// is an exit path a resource may leak across).
    EarlyReturn,
    /// An abort/throw site (`panic!`, Go `panic`, `raise`, `throw`).
    Panic,
    /// A deferred cleanup registration (Go `defer`).
    Defer,
    /// A concurrent task launch (`tokio::spawn`, Go `go`, `.spawn`).
    Spawn,
    /// A lock is taken (`.lock()`, `.Lock()`, `.acquire()`).
    LockAcquire,
    /// A lock is released (`.unlock()`, `.Unlock()`, `.release()`).
    LockRelease,
}

/// One classified tier-2 site: a grammar node whose kind or call target maps
/// to a [`Tier2Family`], carrying its byte/line range, a content anchor, and
/// the exact source slice. `region_hash` is byte-identical to
/// [`region::hash_region`] over the same range (the anchor Hylla stores), so a
/// host can correlate a site with a stored node. `name` is best-effort: the
/// text of the site node's FIRST NAMED child — the callee for call/macro/`new`
/// sites, whatever the first named child is for statement forms (e.g. the
/// returned expression of a `return`), or empty when the node has no named
/// child (a bare `return;`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    /// The classified family.
    pub family: Tier2Family,
    /// The grammar node kind that produced the site (e.g. "call_expression").
    pub kind: String,
    /// Best-effort name = the text of the site node's first NAMED child. For
    /// call/macro/`new` sites that is the callee/constructor (`Box::new`,
    /// `os.Open`, `panic`); for statement forms it is whatever the first named
    /// child is (e.g. the returned expression of `return b`), or empty when
    /// the node has no named child (a bare `return;`).
    pub name: String,
    /// 1-based start line of the site node.
    pub start_line: usize,
    /// 1-based end line of the site node.
    pub end_line: usize,
    /// Inclusive start byte offset of the site node.
    pub start_byte: usize,
    /// Exclusive end byte offset of the site node.
    pub end_byte: usize,
    /// Content anchor over the site's byte range; see [`region::hash_region`].
    pub region_hash: String,
    /// The exact source slice for the site's byte range.
    pub content: String,
}

/// Returns every tier-2 [`Site`] under `opened`'s tree, in source order, for
/// the supported languages (Rust, Go, Python, TS/TSX/JS); any other language —
/// or the grammar-free text fallback — yields an empty vec, the opt-out path.
///
/// The declaration outline is untouched: this walks the SAME tree but selects
/// control-flow/resource nodes instead of declarations, so `read_blocks` and
/// `outline` stay byte-identical. Sites may overlap (a nested call inside a
/// `defer`/`go`/closure is emitted independently), matching the substrate's
/// "every site is a fact" model.
pub fn tier2_sites(opened: &OpenedFile) -> Vec<Site> {
    let lang = opened.lang;
    if !has_table(lang) || !opened.tree.has_native() {
        return Vec::new();
    }
    let src = &opened.tree.source;
    let mut out = Vec::new();
    opened.tree.root.walk(&mut |n| {
        if let Some(family) = classify(lang, n, src) {
            // OOB policy (ONE way): a site whose byte range escapes `src` is
            // SKIPPED wholesale — never emitted with an empty-string content
            // and never hashed over an invalid slice. `content` and
            // `region::hash_region` below are computed over the SAME validated
            // range, so they can never disagree (and hash_region, which slices
            // unguarded and would otherwise panic, is only ever reached here
            // with an in-bounds range).
            if n.start_byte > n.end_byte || n.end_byte > src.len() {
                return;
            }
            // Non-UTF8 skip policy (extends the OOB skip above): a site whose
            // byte range is not valid UTF-8 is SKIPPED wholesale, never emitted
            // with a lossy (U+FFFD-substituted) `content`. This guarantees
            // `content` is ALWAYS byte-for-byte the source slice for EVERY
            // emitted site; `region::hash_region` below (same range) therefore
            // never hashes bytes that `content` silently rewrote.
            let content = match std::str::from_utf8(&src[n.start_byte..n.end_byte]) {
                Ok(s) => s.to_owned(),
                Err(_) => return,
            };
            out.push(Site {
                family,
                region_hash: region::hash_region(src, n.start_byte, n.end_byte),
                kind: n.kind.clone(),
                name: callee_text(n, src).unwrap_or_default(),
                start_line: n.start_point.row + 1,
                end_line: n.end_point.row + 1,
                start_byte: n.start_byte,
                end_byte: n.end_byte,
                content,
            });
        }
    });
    out
}

/// Whether `lang` carries a tier-2 classification table. The first-drop set is
/// Rust, Go, Python, and the TS/TSX/JS trio; everything else opts out.
fn has_table(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Rust | Lang::Go | Lang::Python | Lang::TypeScript | Lang::Tsx | Lang::JavaScript
    )
}

/// Classifies a single node into a [`Tier2Family`], or `None` when it is not a
/// site. Statement-form families are keyed on grammar kind per language; call
/// and `new` forms are keyed on the call target extracted by [`call_family`].
fn classify(lang: Lang, n: &Node, src: &[u8]) -> Option<Tier2Family> {
    // Statement / expression kinds, per language.
    match (lang, n.kind.as_str()) {
        (Lang::Rust, "return_expression") => return Some(Tier2Family::EarlyReturn),
        (Lang::Rust, "macro_invocation") => return rust_macro_family(n, src),
        (Lang::Go, "defer_statement") => return Some(Tier2Family::Defer),
        (Lang::Go, "go_statement") => return Some(Tier2Family::Spawn),
        (Lang::Go, "return_statement") => return Some(Tier2Family::EarlyReturn),
        (Lang::Python, "return_statement") => return Some(Tier2Family::EarlyReturn),
        (Lang::Python, "raise_statement") => return Some(Tier2Family::Panic),
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "return_statement") => {
            return Some(Tier2Family::EarlyReturn);
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "throw_statement") => {
            return Some(Tier2Family::Panic);
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "new_expression") => {
            return Some(Tier2Family::Allocation);
        }
        _ => {}
    }
    // Call forms, keyed on the resolved call target UNDER `lang`.
    if is_call_kind(&n.kind) {
        return call_family(lang, n, src);
    }
    None
}

/// The family for a Rust `macro_invocation`, matching the abort-macro set
/// (`panic!`, `unreachable!`, `todo!`, `unimplemented!`) → [`Tier2Family::Panic`];
/// any other macro is not a site. Path-qualified macros (`std::panic!`,
/// `core::panic!`) match on the FINAL `::` segment, so a fully-qualified abort
/// macro is not missed.
fn rust_macro_family(n: &Node, src: &[u8]) -> Option<Tier2Family> {
    let name = callee_text(n, src)?;
    // FINAL `::` segment: `core::panic` → `panic`, bare `panic` → `panic`.
    let last = name.rsplit("::").next().unwrap_or(name.as_str());
    matches!(last, "panic" | "unreachable" | "todo" | "unimplemented").then_some(Tier2Family::Panic)
}

/// Whether `kind` names a call node across the supported grammars: Rust/Go/
/// JS/TS use `call_expression`, Python uses `call`. TS/JS `new` is a distinct
/// kind handled in [`classify`].
fn is_call_kind(kind: &str) -> bool {
    kind == "call_expression" || kind == "call"
}

/// The syntactic SEPARATOR between a callee's receiver and its final segment —
/// the SHAPE the §8b matrix pins per cell (`Box::new` = path-qualified alloc,
/// `m.lock` = dot-qualified lock, bare `drop` = unqualified release).
/// Distinguishing shape is what blocks cross-shape false positives (bare
/// `open`, `Foo::close`, Python `x.open`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    /// Unqualified callee (`drop`, Go `make`, Python `open`).
    Bare,
    /// Dot/selector access (`m.lock`, `os.Open`, `fh.close`).
    Dot,
    /// Rust path separator `::` (`File::open`, `Box::new`, `tokio::spawn`).
    Path,
}

/// A call target split into an optional receiver segment, the invoked
/// method/function name, and the [`Sep`] shape. `Box::new` → (`Box`, `new`,
/// Path); `os.Open` → (`os`, `Open`, Dot); bare `drop` → (None, `drop`,
/// Bare).
struct CallName {
    /// The segment before the final separator, when the callee is qualified.
    receiver: Option<String>,
    /// The final segment: the invoked method or function name.
    method: String,
    /// The separator shape between receiver and `method`.
    sep: Sep,
}

/// Derives a [`CallName`] from the callee NODE'S GRAMMAR KIND, not by scanning
/// text — the shape is a STRUCTURAL fact of the tree, so `std::io::stdout().lock()`
/// (a `field_expression` whose object text HAPPENS to contain `::`) is Dot, not
/// falsely Path. Per-grammar member-access kinds ([`dot_kind`]) → [`Sep::Dot`]
/// with the member expression's object (first named child) as receiver and the
/// trailing member as method; Rust `scoped_identifier` → [`Sep::Path`] with the
/// final path segment before the method as receiver (turbofish/qualified-generic
/// wrappers unwrapped by [`path_tail_segment`], so `Box::<T>::new` and
/// `<F as C>::spawn` fall out naturally); a bare `identifier` callee →
/// [`Sep::Bare`]. `None` only when a child slice is out of bounds/non-UTF8.
fn split_call(lang: Lang, callee: &Node, src: &[u8]) -> Option<CallName> {
    let named: Vec<&Node> = callee.children.iter().filter(|c| c.named).collect();
    if callee.kind == dot_kind(lang) {
        // Member access: `obj . member`. method = trailing member (last named
        // child); receiver = the object expression (first named child).
        let method = node_text(named.last()?, src)?;
        let receiver = named.first().and_then(|r| node_text(r, src));
        return Some(CallName {
            receiver,
            method,
            sep: Sep::Dot,
        });
    }
    if lang == Lang::Rust && callee.kind == "scoped_identifier" {
        // `path :: name`. method = final `name` (last named child); receiver =
        // the final segment of the path (second-to-last named child), with
        // generic wrappers unwrapped so `Box::<T>` → `Box`, `<F as C>` → text.
        let method = node_text(named.last()?, src)?;
        let receiver = (named.len() >= 2)
            .then(|| path_tail_segment(named[named.len() - 2], src))
            .flatten();
        return Some(CallName {
            receiver,
            method,
            sep: Sep::Path,
        });
    }
    // Bare `identifier` (or any other callee shape): unqualified.
    Some(CallName {
        receiver: None,
        method: node_text(callee, src)?,
        sep: Sep::Bare,
    })
}

/// The per-grammar node kind for member/attribute access — the [`Sep::Dot`]
/// shape. Rust `field_expression`, Go `selector_expression`, Python `attribute`,
/// TS/TSX/JS `member_expression`; other langs return `""` (never matches).
fn dot_kind(lang: Lang) -> &'static str {
    match lang {
        Lang::Rust => "field_expression",
        Lang::Go => "selector_expression",
        Lang::Python => "attribute",
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => "member_expression",
        _ => "",
    }
}

/// The final path segment of a Rust `::` path-receiver node, unwrapping generic
/// noise so the receiver is the real type/module: `scoped_identifier`/
/// `scoped_type_identifier` → their trailing name, `generic_type` (`Box::<T>`)
/// → its leading type (`Box`, type-arguments skipped), a plain identifier → its
/// text. Anything else (e.g. `bracketed_type` `<F as C>`) → its raw text, which
/// deliberately matches none of the receiver-gated tables (alloc/spawn).
fn path_tail_segment(node: &Node, src: &[u8]) -> Option<String> {
    match node.kind.as_str() {
        "scoped_identifier" | "scoped_type_identifier" => {
            // Trailing `name` = last named child.
            let last = node.children.iter().rev().find(|c| c.named)?;
            path_tail_segment(last, src)
        }
        // `Box::<T>` = generic_type[type_identifier, type_arguments]: take the
        // leading type, skipping the turbofish/type-argument node.
        "generic_type" => {
            let first = node.children.iter().find(|c| c.named)?;
            path_tail_segment(first, src)
        }
        _ => node_text(node, src),
    }
}

/// The raw source text of any node, bounds-guarded; `None` on OOB.
fn node_text(node: &Node, src: &[u8]) -> Option<String> {
    if node.end_byte < node.start_byte || node.end_byte > src.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&src[node.start_byte..node.end_byte]).into_owned())
}

/// The family for a call node, resolved from its [`CallName`] under
/// PER-LANGUAGE tables that mirror HYLLA_NODE_CONTRACT.md §8b exactly.
/// Classification is language-scoped — a target that is a site in one grammar
/// is inert in another and NEVER leaks across languages: Go's `make`/`new`
/// builtins and its `Lock`/`RLock`/`TryLock`/`Unlock`/`RUnlock` methods,
/// Python's `acquire`/`release`, Rust's `Box`/`Rc`/`Arc`/`Vec`-class
/// allocations, and TS's case-exact `*.lock` are each confined to their own
/// arm. A bare `new(…)`/`make(…)` is never a Rust allocation; a
/// `threading.Lock()` construction is never a Python lock acquire (only
/// `*.acquire` is). An unrecognized target is not a site (no coerced family).
fn call_family(lang: Lang, n: &Node, src: &[u8]) -> Option<Tier2Family> {
    use Sep::{Bare, Dot, Path};
    let callee = n.children.iter().find(|c| c.named)?;
    let CallName {
        receiver,
        method,
        sep,
    } = split_call(lang, callee, src)?;
    let m = method.as_str();
    Some(match lang {
        // Rust (§8b): acquire = path-qualified `*::{open,create}` (bare `open`
        // / dot `b.create` are NOT); release = bare `drop(…)` OR dot `*.close`
        // (path `Foo::close` is NOT); lock = dot `*.{lock,try_lock}`; spawn =
        // dot `*.spawn` OR path `{tokio,thread}::spawn` ONLY (ORCH RULING:
        // `Whatever::spawn` / `<F as C>::spawn` are NOT sites — the path
        // receiver segment must be `tokio` or `thread`); alloc = path
        // `Box/Rc/Arc/Vec::{new,with_capacity}`. Panic = macro.
        Lang::Rust => match (m, sep) {
            ("drop", Bare) => Tier2Family::ResourceRelease,
            ("close", Dot) => Tier2Family::ResourceRelease,
            ("open" | "create", Path) => Tier2Family::ResourceAcquisition,
            ("lock" | "try_lock", Dot) => Tier2Family::LockAcquire,
            ("spawn", Dot) => Tier2Family::Spawn,
            ("spawn", Path) if matches!(receiver.as_deref(), Some("tokio" | "thread")) => {
                Tier2Family::Spawn
            }
            ("new" | "with_capacity", Path)
                if matches!(receiver.as_deref(), Some("Box" | "Rc" | "Arc" | "Vec")) =>
            {
                Tier2Family::Allocation
            }
            _ => return None,
        },
        // Go (§8b): dot-qualified capitalized methods Close/Open/Create/
        // OpenFile/Dial/Listen + Lock/RLock/TryLock + Unlock/RUnlock (a
        // `x.close()` dot-lowercase is NOT release, a bare `Lock()`/`Open()` is
        // NOT a site); bare builtins `close(…)` release, `panic(…)`, and
        // `make`/`new` allocate.
        Lang::Go => match (m, sep) {
            ("Close", Dot) => Tier2Family::ResourceRelease,
            ("close", Bare) => Tier2Family::ResourceRelease,
            ("Open" | "Create" | "OpenFile" | "Dial" | "Listen", Dot) => {
                Tier2Family::ResourceAcquisition
            }
            ("Lock" | "RLock" | "TryLock", Dot) => Tier2Family::LockAcquire,
            ("Unlock" | "RUnlock", Dot) => Tier2Family::LockRelease,
            ("panic", Bare) => Tier2Family::Panic,
            ("make" | "new", Bare) => Tier2Family::Allocation,
            _ => return None,
        },
        // Python (§8b): `open(…)` acquires ONLY when BARE (`webbrowser.open` /
        // `x.open()` dot forms are NOT); `*.close` releases; lock = dot
        // `*.acquire` ONLY (construction is not acquisition); release = dot
        // `*.release`; spawn = dot `*.create_task`. `raise` is a statement.
        Lang::Python => match (m, sep) {
            ("close", Dot) => Tier2Family::ResourceRelease,
            ("open", Bare) => Tier2Family::ResourceAcquisition,
            ("acquire", Dot) => Tier2Family::LockAcquire,
            ("release", Dot) => Tier2Family::LockRelease,
            ("create_task", Dot) => Tier2Family::Spawn,
            _ => return None,
        },
        // TS/TSX/JS (§8b): ALL call sites are dot-qualified (`*.`); a bare
        // `lock()`/`open()`/`spawn()`/… is NOT a site. `*.close` releases;
        // open/openSync/createReadStream/createWriteStream acquire; case-exact
        // `*.lock`/`*.unlock`; spawn = `*.{spawn,fork}`. `new`/`throw` above.
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => match (m, sep) {
            ("close", Dot) => Tier2Family::ResourceRelease,
            ("open" | "openSync" | "createReadStream" | "createWriteStream", Dot) => {
                Tier2Family::ResourceAcquisition
            }
            ("lock", Dot) => Tier2Family::LockAcquire,
            ("unlock", Dot) => Tier2Family::LockRelease,
            ("spawn" | "fork", Dot) => Tier2Family::Spawn,
            _ => return None,
        },
        _ => return None,
    })
}

/// The raw source text of a call/macro/new node's callee — its first named
/// child (the function/constructor/macro-name node) — bounds-guarded. Empty
/// when there is no named child or the slice is out of bounds.
fn callee_text(n: &Node, src: &[u8]) -> Option<String> {
    let c = n.children.iter().find(|c| c.named)?;
    if c.end_byte < c.start_byte || c.end_byte > src.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&src[c.start_byte..c.end_byte]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::{open_file, read_blocks};

    fn write_temp(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> String {
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// All sites of `content` written under `name`, plus the source bytes so a
    /// test can assert byte-exact ranges and content slices.
    fn sites_of(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> (Vec<Site>, Vec<u8>) {
        let p = write_temp(dir, name, content);
        let opened = open_file(&p).unwrap();
        (tier2_sites(&opened), content.to_vec())
    }

    /// (family, name) pairs for compact presence assertions.
    fn fam_names(sites: &[Site]) -> Vec<(Tier2Family, &str)> {
        sites.iter().map(|s| (s.family, s.name.as_str())).collect()
    }

    #[test]
    fn rust_sites_classified_with_byte_exact_ranges_and_hash_parity() {
        let dir = tempfile::tempdir().unwrap();
        let src = b"fn f(m: std::sync::Mutex<i32>) -> Box<i32> {\n    let b = Box::new(1);\n    let _f = File::open(\"p\");\n    let g = m.lock();\n    drop(g);\n    tokio::spawn(async {});\n    if b.is_null() {\n        panic!(\"no\");\n        return b;\n    }\n    b\n}\n";
        let (sites, bytes) = sites_of(&dir, "m.rs", src);
        let fn_ = fam_names(&sites);
        assert!(
            fn_.contains(&(Tier2Family::Allocation, "Box::new")),
            "{fn_:?}"
        );
        assert!(
            fn_.contains(&(Tier2Family::ResourceAcquisition, "File::open")),
            "{fn_:?}"
        );
        assert!(
            fn_.contains(&(Tier2Family::LockAcquire, "m.lock")),
            "{fn_:?}"
        );
        assert!(
            fn_.contains(&(Tier2Family::ResourceRelease, "drop")),
            "{fn_:?}"
        );
        assert!(
            fn_.contains(&(Tier2Family::Spawn, "tokio::spawn")),
            "{fn_:?}"
        );
        assert!(fn_.iter().any(|(f, _)| *f == Tier2Family::Panic), "{fn_:?}");
        assert!(
            fn_.iter().any(|(f, _)| *f == Tier2Family::EarlyReturn),
            "{fn_:?}"
        );
        // Byte-exact range + region_hash parity for the allocation site.
        let alloc = sites
            .iter()
            .find(|s| s.family == Tier2Family::Allocation)
            .unwrap();
        assert_eq!(&bytes[alloc.start_byte..alloc.end_byte], b"Box::new(1)");
        assert_eq!(alloc.content, "Box::new(1)");
        assert_eq!(
            alloc.region_hash,
            region::hash_region(&bytes, alloc.start_byte, alloc.end_byte)
        );
        assert_eq!(alloc.region_hash.len(), 16);
    }

    #[test]
    fn go_defer_go_panic_open() {
        let dir = tempfile::tempdir().unwrap();
        let src = b"package main\n\nimport \"os\"\n\nfunc f() {\n\tf, _ := os.Open(\"p\")\n\tdefer f.Close()\n\tgo work()\n\tpanic(\"x\")\n}\n";
        let (sites, _) = sites_of(&dir, "m.go", src);
        let fams: Vec<Tier2Family> = sites.iter().map(|s| s.family).collect();
        assert!(fams.contains(&Tier2Family::Defer), "{fams:?}");
        assert!(fams.contains(&Tier2Family::Spawn), "{fams:?}");
        assert!(fams.contains(&Tier2Family::Panic), "{fams:?}");
        assert!(fams.contains(&Tier2Family::ResourceAcquisition), "{fams:?}");
        // Deferred close is ALSO a release site (overlap is intended).
        assert!(fams.contains(&Tier2Family::ResourceRelease), "{fams:?}");
    }

    #[test]
    fn python_open_raise_lock_return() {
        let dir = tempfile::tempdir().unwrap();
        let src =
            b"def f(lock):\n    fh = open('p')\n    lock.acquire()\n    lock.release()\n    if fh:\n        raise ValueError('x')\n    return fh\n";
        let (sites, _) = sites_of(&dir, "m.py", src);
        let fams: Vec<Tier2Family> = sites.iter().map(|s| s.family).collect();
        assert!(fams.contains(&Tier2Family::ResourceAcquisition), "{fams:?}");
        assert!(fams.contains(&Tier2Family::LockAcquire), "{fams:?}");
        assert!(fams.contains(&Tier2Family::LockRelease), "{fams:?}");
        assert!(fams.contains(&Tier2Family::Panic), "{fams:?}");
        assert!(fams.contains(&Tier2Family::EarlyReturn), "{fams:?}");
    }

    #[test]
    fn ts_new_throw_return_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let src = b"function f(): number {\n  const x = new Map();\n  const w = cp.spawn('ls');\n  if (!x) { throw new Error('e'); }\n  return 1;\n}\n";
        let (sites, _) = sites_of(&dir, "m.ts", src);
        let fams: Vec<Tier2Family> = sites.iter().map(|s| s.family).collect();
        assert!(fams.contains(&Tier2Family::Allocation), "{fams:?}");
        assert!(fams.contains(&Tier2Family::Spawn), "{fams:?}");
        assert!(fams.contains(&Tier2Family::Panic), "{fams:?}");
        assert!(fams.contains(&Tier2Family::EarlyReturn), "{fams:?}");
    }

    #[test]
    fn unsupported_langs_return_empty_without_error() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("a.css", &b".x { color: red; }\n"[..]),
            ("n.txt", &b"free text, not code\n"[..]),
        ] {
            let (sites, _) = sites_of(&dir, name, body);
            assert!(sites.is_empty(), "{name} must opt out");
        }
    }

    #[test]
    fn every_site_range_is_a_valid_source_slice() {
        let dir = tempfile::tempdir().unwrap();
        let src = b"fn f() {\n    let b = Box::new(1);\n    drop(b);\n    return;\n}\n";
        let (sites, bytes) = sites_of(&dir, "m.rs", src);
        assert!(!sites.is_empty());
        for s in &sites {
            assert!(s.end_byte <= bytes.len());
            assert!(s.start_byte <= s.end_byte);
            assert_eq!(
                s.content.as_bytes(),
                &bytes[s.start_byte..s.end_byte],
                "content must be the exact slice for {:?}",
                s.kind
            );
            assert!(s.start_line >= 1 && s.end_line >= s.start_line);
        }
    }

    #[test]
    fn adversarial_nested_closures_match_arm_defer_in_defer() {
        let dir = tempfile::tempdir().unwrap();
        // Rust: alloc inside a nested closure, lock in a match arm.
        let rs = b"fn f(m: std::sync::Mutex<i32>, x: i32) {\n    let c = || { let _b = Box::new(1); };\n    c();\n    match x {\n        0 => { let _g = m.lock(); }\n        _ => {}\n    }\n}\n";
        let (sites, _) = sites_of(&dir, "adv.rs", rs);
        let fams: Vec<Tier2Family> = sites.iter().map(|s| s.family).collect();
        assert!(
            fams.contains(&Tier2Family::Allocation),
            "closure alloc {fams:?}"
        );
        assert!(
            fams.contains(&Tier2Family::LockAcquire),
            "arm lock {fams:?}"
        );

        // Go: defer inside a deferred closure — both defers are sites.
        let go = b"package main\n\nfunc f() {\n\tdefer func() {\n\t\tdefer g()\n\t}()\n}\n";
        let (gsites, _) = sites_of(&dir, "adv.go", go);
        let defers = gsites
            .iter()
            .filter(|s| s.family == Tier2Family::Defer)
            .count();
        assert_eq!(defers, 2, "defer-in-defer yields two Defer sites");
    }

    #[test]
    fn every_site_is_contained_by_a_decl_span_or_file_root() {
        // Containment lock: a tier-2 site range NEVER escapes the file root
        // (0..=len). A site INSIDE a declaration lies fully within that decl's
        // span (`read_blocks` is the decl floor); a MODULE-LEVEL site lies
        // outside every decl span yet still inside the file. This pins site
        // geometry to the outline. (No tautological `end_byte <= len` disjunct:
        // that is the bounds guarantee, asserted once, not a containment mode.)
        let dir = tempfile::tempdir().unwrap();
        let src = b"fn f(m: std::sync::Mutex<i32>) {\n    let b = Box::new(1);\n    let g = m.lock();\n    drop(g);\n    return;\n}\n";
        let p = write_temp(&dir, "c.rs", src);
        let opened = open_file(&p).unwrap();
        let sites = tier2_sites(&opened);
        let blocks = read_blocks(&opened, false);
        assert!(!sites.is_empty());
        for s in &sites {
            // File-root bound: never escapes the file.
            assert!(
                s.start_byte <= s.end_byte && s.end_byte <= src.len(),
                "site {:?} escapes file root",
                s.kind
            );
            // Every site here lives inside `fn f`, so decl containment holds.
            let in_decl = blocks
                .iter()
                .any(|b| b.start_byte <= s.start_byte && s.end_byte <= b.end_byte);
            assert!(in_decl, "site {:?} not within the fn f decl span", s.kind);
        }

        // Module-level fixture: a top-level Python `open(...)` is a real site
        // that lies OUTSIDE every declaration block (no enclosing def/class)
        // yet still within the file root — the "file-root, not decl" mode.
        let py = b"fh = open('top')\ndef g():\n    return fh\n";
        let pp = write_temp(&dir, "root.py", py);
        let po = open_file(&pp).unwrap();
        let psites = tier2_sites(&po);
        let pblocks = read_blocks(&po, false);
        let acq = psites
            .iter()
            .find(|s| s.family == Tier2Family::ResourceAcquisition)
            .expect("top-level open() must be an acquisition site");
        // In file root...
        assert!(acq.start_byte <= acq.end_byte && acq.end_byte <= py.len());
        // ...but contained by NO declaration block.
        assert!(
            !pblocks
                .iter()
                .any(|b| b.start_byte <= acq.start_byte && acq.end_byte <= b.end_byte),
            "module-level open() must lie outside every decl span, blocks={pblocks:?}"
        );
    }

    #[test]
    fn cross_language_targets_never_leak_across_grammars() {
        let dir = tempfile::tempdir().unwrap();
        // Python: Go builtin `make`, `threading.Lock()` construction (NOT
        // acquisition), and Go's `Dial` all classify to nothing.
        let py = b"def f(cfg, conn):\n    x = make(cfg)\n    l = threading.Lock()\n    c = conn.Dial()\n    return x\n";
        let (psites, _) = sites_of(&dir, "neg.py", py);
        let pfams: Vec<Tier2Family> = psites.iter().map(|s| s.family).collect();
        assert!(
            !pfams.contains(&Tier2Family::Allocation),
            "python make() is not allocation {pfams:?}"
        );
        assert!(
            !pfams.contains(&Tier2Family::LockAcquire),
            "threading.Lock() construction is not lock acquire {pfams:?}"
        );
        assert!(
            !pfams.contains(&Tier2Family::ResourceAcquisition),
            "python conn.Dial() is not acquisition {pfams:?}"
        );

        // Rust: bare `new(1)` is not allocation; `y.Close()` (capitalized) is
        // not a Rust release.
        let rs = b"fn f(y: T) {\n    let a = new(1);\n    y.Close();\n}\n";
        let (rsites, _) = sites_of(&dir, "neg.rs", rs);
        let rfams: Vec<Tier2Family> = rsites.iter().map(|s| s.family).collect();
        assert!(
            !rfams.contains(&Tier2Family::Allocation),
            "rust bare new() is not allocation {rfams:?}"
        );
        assert!(
            !rfams.contains(&Tier2Family::ResourceRelease),
            "rust y.Close() is not release {rfams:?}"
        );

        // Go: Python's `*.acquire` is not a Go site.
        let go = b"package main\n\nfunc f(p Lock) {\n\tp.acquire()\n}\n";
        let (gsites, _) = sites_of(&dir, "neg.go", go);
        let gfams: Vec<Tier2Family> = gsites.iter().map(|s| s.family).collect();
        assert!(
            !gfams.contains(&Tier2Family::LockAcquire),
            "go p.acquire() is not lock acquire {gfams:?}"
        );

        // TS: `*.Lock` (capitalized) is not a TS lock — only case-exact `.lock`.
        let ts = b"function f(lk: L): number {\n  lk.Lock();\n  return 1;\n}\n";
        let (tsites, _) = sites_of(&dir, "neg.ts", ts);
        let tfams: Vec<Tier2Family> = tsites.iter().map(|s| s.family).collect();
        assert!(
            !tfams.contains(&Tier2Family::LockAcquire),
            "ts lk.Lock() is not lock acquire {tfams:?}"
        );
    }

    #[test]
    fn outline_unaffected_by_tier2_surface() {
        // The declaration floor a host anchors on is independent of tier-2:
        // read_blocks over the same file lists declarations only.
        let dir = tempfile::tempdir().unwrap();
        let p = write_temp(&dir, "m.rs", b"fn f() {\n    return;\n}\n");
        let opened = open_file(&p).unwrap();
        let blocks = read_blocks(&opened, false);
        assert!(
            blocks
                .iter()
                .any(|b| b.name == "f" && b.kind == "function_item")
        );
        // return is a tier-2 site, never a declaration block.
        assert!(!blocks.iter().any(|b| b.kind == "return_expression"));
        assert!(
            tier2_sites(&opened)
                .iter()
                .any(|s| s.family == Tier2Family::EarlyReturn)
        );
    }

    /// True iff `content` under `name` yields a site of `family`.
    fn has_fam(dir: &tempfile::TempDir, name: &str, content: &[u8], family: Tier2Family) -> bool {
        let (sites, _) = sites_of(dir, name, content);
        sites.iter().any(|s| s.family == family)
    }

    #[test]
    fn rust_call_shape_matrix_positive_and_negative() {
        use Tier2Family::*;
        let dir = tempfile::tempdir().unwrap();
        // POSITIVE: each §8b Rust cell, exact shape.
        for (body, fam) in [
            ("let _ = File::open(\"p\");", ResourceAcquisition), // *::open
            ("let _ = X::create(\"p\");", ResourceAcquisition),  // *::create
            ("drop(g);", ResourceRelease),                       // bare drop
            ("m.close();", ResourceRelease),                     // *.close
            ("let _ = m.lock();", LockAcquire),                  // *.lock
            ("let _ = m.try_lock();", LockAcquire),              // *.try_lock
            ("tokio::spawn(async {});", Spawn),                  // path spawn
            ("h.spawn();", Spawn),                               // dot spawn
            ("let _ = Box::new(1);", Allocation),                // path alloc
            ("let _ = Vec::with_capacity(4);", Allocation),      // path alloc
        ] {
            let src = format!("fn f(m: T, g: T, h: T) {{\n    {body}\n}}\n");
            assert!(
                has_fam(&dir, "p.rs", src.as_bytes(), fam),
                "expected {fam:?} for `{body}`"
            );
        }
        // NEGATIVE: the falsifier's counterexample list — wrong shape ≠ site.
        let neg = b"fn f(b: T, m: T) {\n    open(m);\n    create(m);\n    lock(m);\n    spawn(m);\n    close(m);\n    b.create();\n    Foo::close(m);\n}\n";
        let (sites, _) = sites_of(&dir, "neg.rs", neg);
        let fams: Vec<Tier2Family> = sites.iter().map(|s| s.family).collect();
        for f in [
            ResourceAcquisition,
            ResourceRelease,
            LockAcquire,
            Spawn,
            Allocation,
        ] {
            assert!(
                !fams.contains(&f),
                "bare/wrong-shape must not yield {f:?}: {fams:?}"
            );
        }
    }

    #[test]
    fn go_call_shape_matrix_positive_and_negative() {
        use Tier2Family::*;
        let dir = tempfile::tempdir().unwrap();
        for (body, fam) in [
            ("f.Close()", ResourceRelease), // *.Close
            ("close(x)", ResourceRelease),  // bare close builtin
            ("os.Open(\"p\")", ResourceAcquisition),
            ("os.Create(\"p\")", ResourceAcquisition),
            ("os.OpenFile(\"p\")", ResourceAcquisition),
            ("net.Dial(\"t\")", ResourceAcquisition),
            ("net.Listen(\"t\")", ResourceAcquisition),
            ("mu.Lock()", LockAcquire),
            ("mu.RLock()", LockAcquire),
            ("mu.TryLock()", LockAcquire),
            ("mu.Unlock()", LockRelease),
            ("mu.RUnlock()", LockRelease),
            ("panic(\"x\")", Panic),
            ("make(t)", Allocation),
            ("new(t)", Allocation),
        ] {
            let src = format!(
                "package main\n\nfunc g(f T, x T, mu T, os T, net T, t T) {{\n\t{body}\n}}\n"
            );
            assert!(
                has_fam(&dir, "p.go", src.as_bytes(), fam),
                "expected {fam:?} for `{body}`"
            );
        }
        // NEGATIVE: dot-lowercase close, bare capitalized methods ≠ site.
        let neg = b"package main\n\nfunc g(x T) {\n\tx.close()\n\tLock()\n\tUnlock()\n\tOpen()\n\tDial()\n}\n";
        let (sites, _) = sites_of(&dir, "neg.go", neg);
        let fams: Vec<Tier2Family> = sites.iter().map(|s| s.family).collect();
        for f in [
            ResourceRelease,
            LockAcquire,
            LockRelease,
            ResourceAcquisition,
        ] {
            assert!(
                !fams.contains(&f),
                "go wrong-shape must not yield {f:?}: {fams:?}"
            );
        }
    }

    #[test]
    fn python_call_shape_matrix_positive_and_negative() {
        use Tier2Family::*;
        let dir = tempfile::tempdir().unwrap();
        for (body, fam) in [
            ("fh = open('p')", ResourceAcquisition), // bare open ONLY
            ("fh.close()", ResourceRelease),         // *.close
            ("lock.acquire()", LockAcquire),         // *.acquire
            ("lock.release()", LockRelease),         // *.release
            ("loop.create_task(c)", Spawn),          // *.create_task
        ] {
            let src = format!("def g(fh, lock, loop, c):\n    {body}\n");
            assert!(
                has_fam(&dir, "p.py", src.as_bytes(), fam),
                "expected {fam:?} for `{body}`"
            );
        }
        // NEGATIVE: dot `open` (webbrowser.open / x.open) ≠ acquisition; bare
        // close/acquire/release/create_task ≠ site.
        let neg = b"def g(x, webbrowser, u):\n    webbrowser.open(u)\n    x.open()\n    close()\n    acquire()\n    release()\n    create_task()\n";
        let (sites, _) = sites_of(&dir, "neg.py", neg);
        let fams: Vec<Tier2Family> = sites.iter().map(|s| s.family).collect();
        for f in [
            ResourceAcquisition,
            ResourceRelease,
            LockAcquire,
            LockRelease,
            Spawn,
        ] {
            assert!(
                !fams.contains(&f),
                "python wrong-shape must not yield {f:?}: {fams:?}"
            );
        }
    }

    #[test]
    fn ts_call_shape_matrix_positive_and_negative() {
        use Tier2Family::*;
        let dir = tempfile::tempdir().unwrap();
        for (body, fam) in [
            ("f.close();", ResourceRelease),
            ("fs.open();", ResourceAcquisition),
            ("fs.openSync();", ResourceAcquisition),
            ("fs.createReadStream();", ResourceAcquisition),
            ("fs.createWriteStream();", ResourceAcquisition),
            ("m.lock();", LockAcquire),
            ("m.unlock();", LockRelease),
            ("cp.spawn('ls');", Spawn),
            ("cp.fork('w');", Spawn),
        ] {
            let src = format!("function g(f, fs, m, cp) {{\n  {body}\n}}\n");
            assert!(
                has_fam(&dir, "p.ts", src.as_bytes(), fam),
                "expected {fam:?} for `{body}`"
            );
        }
        // NEGATIVE: every TS site is dot-qualified — bare calls ≠ site.
        let neg = b"function g() {\n  lock();\n  unlock();\n  close();\n  open();\n  spawn();\n  fork();\n}\n";
        let (sites, _) = sites_of(&dir, "neg.ts", neg);
        let fams: Vec<Tier2Family> = sites.iter().map(|s| s.family).collect();
        for f in [
            LockAcquire,
            LockRelease,
            ResourceRelease,
            ResourceAcquisition,
            Spawn,
        ] {
            assert!(
                !fams.contains(&f),
                "ts bare call must not yield {f:?}: {fams:?}"
            );
        }
    }

    #[test]
    fn turbofish_receiver_walks_past_generic_args() {
        // `Box::<i32>::new` / `Vec::<u8>::with_capacity`: the turbofish segment
        // (`<i32>`) is skipped so the receiver is the real type (`Box`/`Vec`)
        // and the call classifies Allocation.
        let dir = tempfile::tempdir().unwrap();
        let src =
            b"fn f() {\n    let a = Box::<i32>::new(1);\n    let b = Vec::<u8>::with_capacity(4);\n}\n";
        let (sites, _) = sites_of(&dir, "tf.rs", src);
        let allocs = sites
            .iter()
            .filter(|s| s.family == Tier2Family::Allocation)
            .count();
        assert_eq!(allocs, 2, "both turbofish allocations classify: {sites:?}");
    }

    #[test]
    fn mixed_shape_calls_classify_by_ast_kind_not_text_scan() {
        use Tier2Family::*;
        // Each callee's TEXT contains `::` yet its call SHAPE is Dot (a member
        // access whose object happens to be a path). The AST-kind fix must read
        // the shape from the `field_expression` node, never from `text.contains("::")`.
        let dir = tempfile::tempdir().unwrap();
        for (body, fam) in [
            ("std::io::stdout().lock();", LockAcquire), // Dot lock on path object
            ("std::process::Command::new(\"x\").spawn();", Spawn), // Dot spawn
            ("crate::db::conn().close();", ResourceRelease), // Dot close
            ("x::y().close();", ResourceRelease),       // Dot close, path object
            ("a.b.m.lock();", LockAcquire),             // deep dot chain
            ("Box::<std::string::String>::new(1);", Allocation), // qualified generic
            ("Vec::<std::io::Error>::with_capacity(2);", Allocation), // qualified generic
        ] {
            let src = format!("fn f() {{\n    {body}\n}}\n");
            assert!(
                has_fam(&dir, "mix.rs", src.as_bytes(), fam),
                "expected {fam:?} for `{body}`"
            );
        }
        // `std::process::Command::new(...)` is ALSO a path `Command::new` — but
        // receiver `Command` is not in the alloc set, so it is not Allocation.
        let (sites, _) = sites_of(
            &dir,
            "cmd.rs",
            b"fn f() {\n    std::process::Command::new(\"x\");\n}\n",
        );
        assert!(
            !sites.iter().any(|s| s.family == Allocation),
            "Command::new is not an allocation: {sites:?}"
        );
    }

    #[test]
    fn qualified_generic_receiver_unwraps_to_real_type() {
        // `Box::<std::string::String>::new` / `Vec::<std::io::Error>::with_capacity`:
        // the qualified-generic path element (a `generic_type` wrapping a
        // `type_arguments` that ITSELF contains `::` paths) is unwrapped so the
        // receiver is `Box`/`Vec` and both classify Allocation via the AST path.
        let dir = tempfile::tempdir().unwrap();
        let src = b"fn f() {\n    let a = Box::<std::string::String>::new(1);\n    let b = Vec::<std::io::Error>::with_capacity(2);\n}\n";
        let (sites, _) = sites_of(&dir, "qg.rs", src);
        let allocs = sites
            .iter()
            .filter(|s| s.family == Tier2Family::Allocation)
            .count();
        assert_eq!(
            allocs, 2,
            "both qualified-generic allocs classify: {sites:?}"
        );
    }

    #[test]
    fn rust_path_spawn_only_tokio_or_thread_receiver() {
        use Tier2Family::*;
        let dir = tempfile::tempdir().unwrap();
        // POSITIVE: path spawn with tokio/thread receiver; dot spawn always.
        for body in [
            "tokio::spawn(async {});",
            "thread::spawn(|| {});",
            "h.spawn();",
        ] {
            let src = format!("fn f(h: T) {{\n    {body}\n}}\n");
            assert!(
                has_fam(&dir, "sp.rs", src.as_bytes(), Spawn),
                "expected Spawn for `{body}`"
            );
        }
        // NEGATIVE: path spawn with any other receiver is NOT a site.
        let neg = b"fn f() {\n    Whatever::spawn();\n    <F as C>::spawn();\n}\n";
        let (sites, _) = sites_of(&dir, "spneg.rs", neg);
        assert!(
            !sites.iter().any(|s| s.family == Spawn),
            "Whatever::spawn / <F as C>::spawn must not be Spawn sites: {sites:?}"
        );
    }

    #[test]
    fn rust_qualified_abort_macro_matches_final_segment() {
        use Tier2Family::*;
        let dir = tempfile::tempdir().unwrap();
        // Path-qualified abort macros match on the FINAL `::` segment.
        for body in ["core::panic!(\"x\");", "std::panic!(\"x\");"] {
            let src = format!("fn f() {{\n    {body}\n}}\n");
            assert!(
                has_fam(&dir, "mac.rs", src.as_bytes(), Panic),
                "expected Panic for `{body}`"
            );
        }
        // A non-abort qualified macro is not a site.
        let (sites, _) = sites_of(&dir, "vec.rs", b"fn f() {\n    std::vec![1];\n}\n");
        assert!(
            !sites.iter().any(|s| s.family == Panic),
            "std::vec! is not a Panic site: {sites:?}"
        );
    }

    #[test]
    fn non_utf8_site_range_is_skipped_and_content_is_byte_exact() {
        // A site whose byte range is not valid UTF-8 is SKIPPED (never emitted
        // with a lossy content). The valid neighbours ARE emitted, and every
        // emitted site's `content` equals the exact source slice byte-for-byte.
        let dir = tempfile::tempdir().unwrap();
        let mut src: Vec<u8> = Vec::new();
        src.extend_from_slice(b"fn f() {\n    let b = Box::new(1);\n    let _f = File::open(\"p");
        src.push(0xFF); // invalid UTF-8 byte inside the File::open(...) range
        src.extend_from_slice(b"\");\n    return;\n}\n");
        let (sites, bytes) = sites_of(&dir, "bad.rs", &src);
        // The valid allocation site is still emitted...
        assert!(
            sites.iter().any(|s| s.family == Tier2Family::Allocation),
            "valid Box::new site must still emit: {sites:?}"
        );
        // ...the non-UTF8 File::open(...) acquisition is skipped wholesale.
        assert!(
            !sites
                .iter()
                .any(|s| s.family == Tier2Family::ResourceAcquisition),
            "non-UTF8 acquisition site must be skipped: {sites:?}"
        );
        // Byte-exactness holds for EVERY emitted site (never lossy).
        for s in &sites {
            assert_eq!(
                s.content.as_bytes(),
                &bytes[s.start_byte..s.end_byte],
                "content must equal exact slice for {:?}",
                s.kind
            );
        }
    }
}
