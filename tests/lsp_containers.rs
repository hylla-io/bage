//! Docker-gated LSP integration suite: real language servers in containers,
//! spoken to over stdio via `docker run -i --rm` (Content-Length framing
//! flows straight through the docker CLI, so no TCP bridge is needed).
//!
//! Skipped unless `BAGE_DOCKER_LSP=1`. Each case mounts a temp fixture into
//! the container AT THE SAME ABSOLUTE PATH as on the host, so `file://` URIs,
//! workspace priming's filesystem walk, and the generated
//! compile_commands.json all resolve identically on both sides. Images are
//! pinned; servers not baked into an image are installed at container start,
//! hence the generous timeouts.
//!
//! Run: `BAGE_DOCKER_LSP=1 cargo test --test lsp_containers -- --nocapture`

use std::collections::BTreeSet;
use std::fs;
use std::time::Duration;

use bage::lsp::{self, Client};

/// Reports whether the docker-gated suite is enabled for this run.
fn docker_enabled() -> bool {
    std::env::var("BAGE_DOCKER_LSP").as_deref() == Ok("1")
}

/// One container rename scenario: a fixture tree, the image + server argv to
/// run in it, the rename position, and the files the resulting WorkspaceEdit
/// must touch.
struct Case {
    /// Human name for failure messages.
    name: &'static str,
    /// Pinned container image.
    image: &'static str,
    /// argv after the image (the language-server command, possibly a shell
    /// wrapper that installs it first).
    server: &'static [&'static str],
    /// Fixture files as (relative path, content).
    files: &'static [(&'static str, &'static str)],
    /// Relative path of the file the rename starts in.
    target: &'static str,
    /// Zero-based line of the symbol.
    line: u32,
    /// Zero-based UTF-16 column of the symbol.
    col: u32,
    /// The new symbol name.
    new_name: &'static str,
    /// Relative paths that MUST receive at least one edit.
    expect_in: &'static [&'static str],
}

/// Runs one case end-to-end: builds the fixture, spawns the server container
/// over stdio, initializes at the fixture root, requests the rename through
/// the production `Client::rename` path (which primes the workspace and, for
/// clangd, generates compile_commands.json), and asserts every expected file
/// received an edit.
fn run_case(case: &Case) {
    if !docker_enabled() {
        eprintln!("{}: skipped (set BAGE_DOCKER_LSP=1 to run)", case.name);
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    // Canonicalize so the path docker mounts is the real one (macOS /var ->
    // /private/var) and URIs match on both sides of the mount.
    let root = dir
        .path()
        .canonicalize()
        .expect("canonicalize fixture root");
    for (rel, content) in case.files {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("fixture dirs");
        }
        fs::write(&p, content).expect("fixture file");
    }

    let root_str = root.to_str().expect("utf-8 fixture root");
    let mut command: Vec<String> = [
        "docker",
        "run",
        "-i",
        "--rm",
        "-v",
        &format!("{root_str}:{root_str}"),
        case.image,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    command.extend(case.server.iter().map(|s| s.to_string()));

    let mut client = Client::new_stdio(&command).expect("spawn docker run");
    // Generous bounds: first runs pull images and install servers.
    client.call_timeout = Duration::from_secs(300);
    client.rename_deadline = Duration::from_secs(300);
    client.rename_retry = Duration::from_secs(2);

    client
        .initialize(&lsp::file_uri(root_str).to_string())
        .unwrap_or_else(|e| panic!("{}: initialize: {e}", case.name));

    let target = root.join(case.target);
    let target_str = target.to_str().expect("utf-8 target path");
    let content = fs::read_to_string(&target).expect("read target");
    let we = client
        .rename(target_str, &content, case.line, case.col, case.new_name)
        .unwrap_or_else(|e| panic!("{}: rename: {e}", case.name));

    let edits =
        lsp::workspace_edit_to_file_edits(&we, |p| fs::read(p)).expect("convert WorkspaceEdit");
    let touched: BTreeSet<&str> = edits.iter().map(|e| e.path.as_str()).collect();
    eprintln!("{}: edits touched {touched:?}", case.name);
    for rel in case.expect_in {
        let want = root.join(rel);
        assert!(
            touched.contains(want.to_str().unwrap()),
            "{}: rename must land in {rel} (touched: {touched:?})",
            case.name
        );
    }

    client.close().expect("close");
    assert!(
        !root.join("compile_commands.json").exists() || !command_mentions_clangd(&command),
        "{}: a bage-generated compile_commands.json must be removed on close",
        case.name
    );
}

/// Mirror of the client's clangd detection, for the cleanup assertion.
fn command_mentions_clangd(command: &[String]) -> bool {
    command.iter().any(|t| t.contains("clangd"))
}

/// Verifies the generated compile_commands.json really referenced the
/// fixture TUs before the rename ran (spot check via a fresh generation in a
/// copy — the live one is removed by close). Kept host-only: no docker.
#[test]
fn compile_commands_generation_covers_fixture() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, content) in C_FILES {
        let p = dir.path().join(rel);
        fs::write(&p, content).expect("fixture file");
    }
    let created = lsp::ensure_compile_commands(dir.path())
        .expect("generate")
        .expect("created");
    let body = fs::read_to_string(&created).expect("read db");
    for tu in ["util.c", "main.c"] {
        assert!(body.contains(tu), "database must cover {tu}: {body}");
    }
    assert!(
        !body.contains("util.h"),
        "headers are not translation units: {body}"
    );
}

/// gopls fixture: a two-file module where `Hello` is defined in a.go and
/// referenced in b.go.
const GO_FILES: &[(&str, &str)] = &[
    ("go.mod", "module example.com/m\n\ngo 1.23\n"),
    ("a.go", "package m\n\nfunc Hello() int { return 1 }\n"),
    ("b.go", "package m\n\nfunc Use() int { return Hello() }\n"),
];

/// pyright fixture: `greet` defined in lib.py, imported and called in
/// main.py — the cross-file half only reachable with workspace priming.
const PY_FILES: &[(&str, &str)] = &[
    ("lib.py", "def greet():\n    return 1\n"),
    ("main.py", "from lib import greet\n\nprint(greet())\n"),
];

/// clangd fixture: two translation units sharing a header — cross-TU rename
/// requires the generated compile_commands.json.
const C_FILES: &[(&str, &str)] = &[
    ("util.h", "int add(int a, int b);\n"),
    (
        "util.c",
        "#include \"util.h\"\n\nint add(int a, int b) { return a + b; }\n",
    ),
    (
        "main.c",
        "#include \"util.h\"\n\nint main(void) { return add(1, 2); }\n",
    ),
];

/// gopls: cross-file rename must land in both a.go (definition) and b.go
/// (reference). gopls does full-workspace rename natively; this case anchors
/// the baseline the pyright/clangd fixes are measured against.
#[test]
fn gopls_cross_file_rename() {
    run_case(&Case {
        name: "gopls",
        image: "golang:1.24-bookworm",
        server: &[
            "sh",
            "-lc",
            "go install golang.org/x/tools/gopls@v0.18.1 1>&2 && exec gopls",
        ],
        files: GO_FILES,
        target: "a.go",
        line: 2,
        col: 5,
        new_name: "Howdy",
        expect_in: &["a.go", "b.go"],
    });
}

/// pyright: only considers OPEN files, so this cross-file rename passes only
/// because `Client::rename` primes the workspace (didOpens main.py) first —
/// the issue #23 fix under test.
#[test]
fn pyright_cross_file_rename_with_priming() {
    run_case(&Case {
        name: "pyright",
        image: "node:20-bookworm-slim",
        server: &[
            "sh",
            "-lc",
            "npm install -g pyright@1.1.402 1>&2 && exec pyright-langserver --stdio",
        ],
        files: PY_FILES,
        target: "lib.py",
        line: 0,
        col: 4,
        new_name: "hello",
        expect_in: &["lib.py", "main.py"],
    });
}

/// clangd: without a compilation database each file is an isolated TU and
/// the rename stays single-file; the bage-generated compile_commands.json
/// plus priming must carry it into main.c — the issue #23 fix under test.
#[test]
fn clangd_cross_tu_rename_with_generated_compile_commands() {
    run_case(&Case {
        name: "clangd",
        image: "debian:bookworm-slim",
        server: &[
            "sh",
            "-lc",
            "apt-get update 1>&2 && apt-get install -y --no-install-recommends clangd 1>&2 && exec clangd",
        ],
        files: C_FILES,
        target: "util.c",
        line: 2,
        col: 4,
        new_name: "sum",
        expect_in: &["util.c", "main.c"],
    });
}
