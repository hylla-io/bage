//! Docker-gated LSP integration suite: real language servers in containers,
//! spoken to over stdio via `docker run -i --rm` (Content-Length framing
//! flows straight through the docker CLI, so no TCP bridge is needed).
//!
//! The container cases are `#[ignore]`d so a default run REPORTS them as
//! ignored rather than passed: a case that returns early still scores `ok`,
//! which is indistinguishable from a real cross-file rename having succeeded.
//! Each case mounts a temp fixture into
//! the container AT THE SAME ABSOLUTE PATH as on the host, so `file://` URIs
//! and workspace priming's filesystem walk resolve identically on both
//! sides. Images are
//! pinned; servers not baked into an image are installed at container start,
//! hence the generous timeouts. Wrappers use plain `sh -c`, never a login
//! shell: `/etc/profile` resets `PATH` and drops the image's toolchain
//! directories (golang's `/usr/local/go/bin` and `/go/bin`).
//!
//! Run: `BAGE_DOCKER_LSP=1 cargo test --test lsp_containers -- --ignored
//! --nocapture`

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
    /// A (relative path, line, UTF-16 col) reference that must resolve before
    /// the rename is asked — the `Client::await_ready` gate for a server that
    /// otherwise answers a cold rename with a partial edit.
    ready: Option<(&'static str, u32, u32)>,
}

/// Runs one case end-to-end: builds the fixture, spawns the server container
/// over stdio, initializes at the fixture root, passes the case's declared
/// readiness gate, requests the rename through the production
/// `Client::rename` path (which primes the workspace), and asserts every
/// expected file received an edit.
fn run_case(case: &Case) {
    // Reaching here means the operator selected an `#[ignore]`d case on
    // purpose, so a missing gate is their mistake to see, not one to swallow
    // into a green result.
    assert!(
        docker_enabled(),
        "{}: needs BAGE_DOCKER_LSP=1 (and a running docker daemon)",
        case.name
    );

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
    client.initialize_timeout = Duration::from_secs(300);
    client.call_timeout = Duration::from_secs(300);
    client.rename_deadline = Duration::from_secs(300);
    client.rename_retry = Duration::from_secs(2);
    client.ready_deadline = Duration::from_secs(300);
    // The server runs in the container's pid namespace, where this process's
    // pid does not exist; declared, it reads as a dead parent and the server
    // exits mid-session.
    client.process_id = None;

    client
        .initialize(&lsp::file_uri(root_str).to_string())
        .unwrap_or_else(|e| panic!("{}: initialize: {e}", case.name));

    if let Some((rel, line, col)) = case.ready {
        let probe = root.join(rel);
        let probe_str = probe.to_str().expect("utf-8 probe path");
        let text = fs::read_to_string(&probe).expect("read probe file");
        client
            .await_ready(probe_str, &text, line, col)
            .unwrap_or_else(|e| panic!("{}: await_ready: {e}", case.name));
    }

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

/// gopls: cross-file rename must land in both a.go (definition) and b.go
/// (reference). gopls does full-workspace rename natively; this case anchors
/// the baseline the pyright fix is measured against.
#[test]
#[ignore = "needs docker: BAGE_DOCKER_LSP=1 cargo test --test lsp_containers -- --ignored"]
fn gopls_cross_file_rename() {
    run_case(&Case {
        name: "gopls",
        image: "golang:1.24-bookworm",
        server: &[
            "sh",
            "-c",
            "go install golang.org/x/tools/gopls@v0.18.1 1>&2 && exec gopls",
        ],
        files: GO_FILES,
        target: "a.go",
        line: 2,
        col: 5,
        new_name: "Howdy",
        expect_in: &["a.go", "b.go"],
        ready: None,
    });
}

/// pyright: ignores the deprecated `rootUri`, so this cross-file rename
/// passes only because `Client::initialize` also declares the root as a
/// workspace folder.
#[test]
#[ignore = "needs docker: BAGE_DOCKER_LSP=1 cargo test --test lsp_containers -- --ignored"]
fn pyright_cross_file_rename_with_priming() {
    run_case(&Case {
        name: "pyright",
        image: "node:20-bookworm-slim",
        server: &[
            "sh",
            "-c",
            "npm install -g pyright@1.1.402 1>&2 && exec pyright-langserver --stdio",
        ],
        files: PY_FILES,
        target: "lib.py",
        line: 0,
        col: 4,
        new_name: "hello",
        expect_in: &["lib.py", "main.py"],
        ready: None,
    });
}
