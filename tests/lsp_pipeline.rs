//! Starting a server in its workspace, and pipelined call-hierarchy batches,
//! through the crate's public API only.
//!
//! The working-directory tests spawn `sh` and always run. The rustup and
//! rust-analyzer cases are `#[ignore]`d, so a default run REPORTS them as
//! ignored rather than passed; run them with
//! `BAGE_LSP_REAL_TEST=1 cargo test --test lsp_pipeline -- --ignored`, under
//! which a missing server, or the opt-in left unset, FAILS.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bage::lsp::{self, CallTarget, Client, ClientConfig, LspError, PositionQuery, SymbolLocation};

/// A stdio child that prints the physical directory it started in to stderr,
/// then waits.
fn pwd_server() -> Vec<String> {
    ["sh", "-c", "pwd -P >&2; sleep 30"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The first stderr line, once the drain thread has read one.
fn first_stderr_line(c: &Client) -> String {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let tail = c.stderr_tail();
        if let Some(line) = tail
            .text
            .lines()
            .next()
            .filter(|_| tail.text.contains('\n'))
        {
            return line.to_string();
        }
        assert!(
            Instant::now() < until,
            "the child printed nothing: {tail:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The real-server cases are `#[ignore]`d, so reaching one means the operator
/// selected it on purpose; without the opt-in that is a mistake to see, not
/// one to swallow into a green result.
fn require_real_tier() {
    assert!(
        std::env::var("BAGE_LSP_REAL_TEST").ok().as_deref() == Some("1"),
        "the real-server tier needs BAGE_LSP_REAL_TEST=1 (and rustup and rust-analyzer on PATH)"
    );
}

fn quick_close(mut c: Client) {
    c.configure(ClientConfig {
        shutdown_timeout: Duration::from_millis(50),
        exit_deadline: Duration::from_millis(50),
        ..ClientConfig::default()
    });
    // The child is no language server and cannot answer `shutdown`; close
    // still reaps it, which is all this needs.
    let _ = c.close();
}

#[test]
fn new_stdio_in_starts_the_server_in_the_given_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let want = dir.path().canonicalize().expect("canonical dir");
    let c = Client::new_stdio_in(&pwd_server(), dir.path()).expect("spawn sh");
    assert_eq!(Path::new(&first_stderr_line(&c)), want);
    quick_close(c);

    // The control: without a directory the server starts where this process
    // runs, so the assertion above is not true of every spawn.
    let here = std::env::current_dir()
        .expect("cwd")
        .canonicalize()
        .expect("canonical cwd");
    assert_ne!(here, want, "the control must start somewhere else");
    let c = Client::new_stdio(&pwd_server()).expect("spawn sh");
    assert_eq!(Path::new(&first_stderr_line(&c)), here);
    quick_close(c);
}

#[test]
fn new_stdio_in_refuses_a_directory_that_is_not_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("absent");
    match Client::new_stdio_in(&pwd_server(), &missing) {
        Err(LspError::WorkingDir { dir, source }) => {
            assert_eq!(dir, missing);
            assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!(
            "want WorkingDir for a missing directory, got {:?}",
            other.err()
        ),
    }
    let file = dir.path().join("file");
    fs::write(&file, "").expect("file");
    match Client::new_stdio_in(&pwd_server(), &file) {
        Err(LspError::WorkingDir { dir, source }) => {
            assert_eq!(dir, file);
            assert_eq!(source.kind(), std::io::ErrorKind::NotADirectory);
        }
        other => panic!("want WorkingDir for a file, got {:?}", other.err()),
    }
}

/// The hazard the working directory exists for: rustup's `rust-analyzer`
/// proxy picks its toolchain from the `rust-toolchain.toml` above the
/// directory it starts in. The workspace's file names a local toolchain
/// whose `rust-analyzer` only says where it came from.
#[cfg(unix)]
#[test]
#[ignore = "real rustup: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_pipeline -- --ignored"]
fn rustup_starts_the_toolchain_the_workspace_names() {
    use std::os::unix::fs::PermissionsExt;
    require_real_tier();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical");
    let bin = root.join("toolchain/bin");
    fs::create_dir_all(&bin).expect("toolchain dir");
    let server = bin.join("rust-analyzer");
    fs::write(
        &server,
        "#!/bin/sh\necho \"the workspace's toolchain\" >&2\nexec sleep 30\n",
    )
    .expect("server script");
    fs::set_permissions(&server, fs::Permissions::from_mode(0o755)).expect("chmod");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(
        workspace.join("rust-toolchain.toml"),
        format!(
            "[toolchain]\npath = \"{}\"\n",
            root.join("toolchain").display()
        ),
    )
    .expect("toolchain file");
    // rustup ranks `RUSTUP_TOOLCHAIN` above any toolchain file, and its cargo
    // proxy sets it for everything cargo runs — this test included — so this
    // passes only because the workspace spawn drops the inherited one.
    assert!(
        std::env::var_os("RUSTUP_TOOLCHAIN").is_some(),
        "precondition: run under rustup's cargo, which sets RUSTUP_TOOLCHAIN"
    );
    let command = vec!["rust-analyzer".to_string()];
    let c = Client::new_stdio_in(&command, &workspace)
        .unwrap_or_else(|e| panic!("spawn the rust-analyzer proxy: {e}"));
    assert_eq!(first_stderr_line(&c), "the workspace's toolchain");
    quick_close(c);
}

// ---- rust-analyzer: pipelined answers equal one-at-a-time answers ----

/// A crate whose functions call each other within and across files, with
/// enough of them that a batch spans many windows.
fn write_fixture(root: &Path) {
    let mut chain = String::from("use crate::b::{helper, leaf};\n\n");
    chain.push_str("pub fn f0() -> u32 {\n    helper()\n}\n\n");
    for i in 1..40 {
        chain.push_str(&format!(
            "pub fn f{i}() -> u32 {{\n    f{}() + leaf()\n}}\n\n",
            i - 1
        ));
    }
    let files = [
        (
            "Cargo.toml",
            "[package]\nname = \"t\"\nversion = \"0.0.0\"\nedition = \"2021\"\n".to_string(),
        ),
        ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n".to_string()),
        (
            "src/a.rs",
            "use crate::b::{helper, other};\n\npub fn run() -> u32 {\n    helper() + other()\n}\n\npub fn twice() -> u32 {\n    run() + run()\n}\n"
                .to_string(),
        ),
        (
            "src/b.rs",
            "pub fn helper() -> u32 {\n    leaf() + 1\n}\n\npub fn leaf() -> u32 {\n    1\n}\n\npub fn other() -> u32 {\n    helper() * leaf()\n}\n"
                .to_string(),
        ),
        ("src/c.rs", chain),
    ];
    for (rel, content) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        fs::write(&p, content).expect("fixture");
    }
}

/// Every `fn NAME` in the fixture's sources, as (path, text, line, column of
/// NAME). The fixtures are ASCII, so a byte column is a UTF-16 column.
fn declarations(root: &Path) -> Vec<(String, String, u32, u32)> {
    let mut out = Vec::new();
    for rel in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        let path = root.join(rel);
        let text = fs::read_to_string(&path).expect("read");
        for (line, row) in text.lines().enumerate() {
            if let Some(at) = row.find("fn ") {
                out.push((
                    path.to_str().expect("utf-8").to_string(),
                    text.clone(),
                    line as u32,
                    (at + 3) as u32,
                ));
            }
        }
    }
    out
}

type Edge = (String, SymbolLocation, Vec<SymbolLocation>);

fn target_key(t: &CallTarget) -> (String, SymbolLocation) {
    (t.name.clone(), t.location.clone())
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_pipeline -- --ignored"]
fn pipelined_call_hierarchy_equals_one_at_a_time_against_rust_analyzer() {
    require_real_tier();
    let dir = tempfile::tempdir().expect("tempdir");
    let root: PathBuf = dir.path().canonicalize().expect("canonical root");
    write_fixture(&root);
    let mut c = Client::new_stdio_in(&["rust-analyzer".to_string()], &root)
        .unwrap_or_else(|e| panic!("spawn rust-analyzer: {e}"));
    c.configure(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        query_deadline: Duration::from_secs(60),
        ready_deadline: Duration::from_secs(120),
        ready_retry: Duration::from_millis(250),
        ..ClientConfig::default()
    });
    c.initialize(&lsp::file_uri(root.to_str().expect("utf-8")).to_string())
        .expect("initialize");
    let a = root.join("src/a.rs");
    let a_text = fs::read_to_string(&a).expect("read a.rs");
    // `helper` in `helper() + other()` resolves into b.rs once loaded.
    c.await_ready(a.to_str().expect("utf-8"), &a_text, 3, 4)
        .expect("ready");

    let decls = declarations(&root);
    let queries: Vec<PositionQuery<'_>> = decls
        .iter()
        .map(|(path, content, line, character)| PositionQuery {
            path,
            content,
            line: *line,
            character: *character,
        })
        .collect();

    // One at a time, through the single-query calls.
    let started = Instant::now();
    let mut serial_targets = Vec::new();
    let mut serial_edges: Vec<Vec<Edge>> = Vec::new();
    for q in &queries {
        let targets = c
            .prepare_call_hierarchy(q.path, q.content, q.line, q.character)
            .unwrap_or_else(|e| panic!("{}:{}: {e}", q.path, q.line));
        for t in &targets {
            let calls = c.outgoing_calls(t).expect("outgoing");
            serial_edges.push(
                calls
                    .into_iter()
                    .map(|o| (o.to.name, o.to.location, o.call_sites))
                    .collect(),
            );
        }
        serial_targets.push(targets.iter().map(target_key).collect::<Vec<_>>());
    }
    let serial = started.elapsed();

    // Pipelined, eight in flight.
    let eight = NonZeroUsize::new(8).expect("nonzero");
    let started = Instant::now();
    let prepared = c.prepare_call_hierarchy_many(&queries, eight);
    let mut piped_targets = Vec::new();
    let mut targets: Vec<CallTarget> = Vec::new();
    for (q, answer) in queries.iter().zip(prepared) {
        let got = answer.unwrap_or_else(|e| panic!("{}:{}: {e}", q.path, q.line));
        piped_targets.push(got.iter().map(target_key).collect::<Vec<_>>());
        targets.extend(got);
    }
    let piped_edges: Vec<Vec<Edge>> = c
        .outgoing_calls_many(&targets, eight)
        .into_iter()
        .map(|answer| {
            answer
                .expect("outgoing")
                .into_iter()
                .map(|o| (o.to.name, o.to.location, o.call_sites))
                .collect()
        })
        .collect();
    let piped = started.elapsed();
    eprintln!(
        "rust-analyzer: {} queries, {} targets: one at a time {serial:?}, eight in flight {piped:?}",
        queries.len(),
        targets.len()
    );

    assert_eq!(
        piped_targets, serial_targets,
        "the same targets, in query order"
    );
    assert_eq!(
        piped_edges, serial_edges,
        "the same callees and call sites, per target"
    );

    // Equal empties would prove nothing: the known edges must be there.
    let calls_of = |name: &str| -> Vec<String> {
        let i = piped_targets
            .iter()
            .flatten()
            .position(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no target {name}"));
        let mut names: Vec<String> = piped_edges[i].iter().map(|(n, _, _)| n.clone()).collect();
        names.sort();
        names
    };
    assert_eq!(calls_of("run"), ["helper", "other"], "a.rs run → b.rs");
    assert_eq!(
        calls_of("f39"),
        ["f38", "leaf"],
        "c.rs f39 → c.rs f38 and b.rs leaf"
    );
    assert_eq!(calls_of("f0"), ["helper"]);

    c.close().expect("close");
}
