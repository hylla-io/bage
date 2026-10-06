//! Declared-session bounds and public document sync, driven through the
//! crate's PUBLIC API only — a consumer outside the crate must be able to
//! declare every time bound before `initialize` and drive `didOpen`/`didClose`
//! itself.
//!
//! The bound tests use a real subprocess that speaks no LSP at all (`sleep`):
//! it never answers `initialize` or `shutdown` and never exits on `exit`, which
//! is exactly the server a declared bound must cut short.
//!
//! The real-server cases are `#[ignore]`d, so a default run REPORTS them as
//! ignored rather than passed. Run them with
//! `BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored`; a missing
//! server, or the opt-in left unset, FAILS rather than skipping, because the
//! operator asked for that tier.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bage::lsp::{
    self, Client, ClientConfig, DiagnosticFailure, LspError, LspPool, MarkupKind, ProbeAnswer,
    ReadyFailure, ReadyMode, ReadySignal, Severity,
};

/// A stdio child that reads nothing and answers nothing for longer than any
/// bound under test.
fn silent_server() -> Vec<String> {
    vec!["sleep".to_string(), "30".to_string()]
}

/// `kill -0` liveness probe; POSIX, no extra dependency.
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn default_config_is_the_documented_defaults() {
    let d = ClientConfig::default();
    assert_eq!(d.initialize_timeout, Duration::from_secs(30));
    assert_eq!(d.call_timeout, Duration::from_secs(30));
    assert_eq!(d.shutdown_timeout, Duration::from_secs(2));
    assert_eq!(d.exit_deadline, Duration::from_secs(3));
    assert_eq!(d.exit_poll, Duration::from_millis(50));
    assert_eq!(d.process_id, Some(std::process::id()));
    assert_eq!(d.initialization_options, None);
    assert_eq!(d.experimental_capabilities, None);
    assert!(d.ready_failures.is_empty());
    assert!(d.ready_signals.is_empty());
    assert!(d.diagnostic_failures.is_empty());
    assert_eq!(
        d.hover_content_format,
        [MarkupKind::Markdown, MarkupKind::PlainText]
    );
    assert_eq!(d.stderr_tail_bytes, 16 * 1024);
}

/// rust-analyzer's status extension, as a caller declares it.
fn rust_analyzer_failed() -> ReadyFailure {
    ReadyFailure {
        method: "experimental/serverStatus".to_string(),
        when: serde_json::json!({"health": "error", "quiescent": true}),
        message_pointer: Some("/message".to_string()),
        mode: ReadyMode::Latest,
    }
}

/// rust-analyzer's "done loading", as a caller declares it: a state, re-sent
/// whenever it changes.
fn rust_analyzer_quiescent() -> ReadySignal {
    ReadySignal {
        method: "experimental/serverStatus".to_string(),
        when: serde_json::json!({"quiescent": true}),
        mode: ReadyMode::Latest,
    }
}

/// gopls's "done loading", as a caller declares it: an event, said once on a
/// method that goes on to carry other messages.
fn gopls_finished_loading(mode: ReadyMode) -> ReadySignal {
    ReadySignal {
        method: "window/showMessage".to_string(),
        when: serde_json::json!({"message": "Finished loading packages."}),
        mode,
    }
}

/// A stdio child that writes `count` numbered lines to stderr, then hangs.
fn noisy_server(count: u32) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "i=0; while [ $i -lt {count} ]; do echo \"line $i\" >&2; i=$((i+1)); done; sleep 30"
        ),
    ]
}

/// Polls `stderr_tail` until `want` shows up: the drain thread reads on its
/// own schedule.
fn wait_for_tail(c: &Client, want: &str) -> lsp::StderrTail {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let tail = c.stderr_tail();
        if tail.text.contains(want) || Instant::now() > until {
            return tail;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stderr_tail_is_bounded_and_reports_the_cut() {
    let mut c = Client::new_stdio(&noisy_server(200)).expect("spawn sh");
    c.configure(ClientConfig {
        initialize_timeout: Duration::from_millis(200),
        stderr_tail_bytes: 64,
        shutdown_timeout: Duration::from_millis(50),
        exit_deadline: Duration::from_millis(50),
        ..ClientConfig::default()
    });
    let err = c
        .initialize(&lsp::file_uri("/tmp").to_string())
        .expect_err("a server that never answers cannot initialize");
    assert!(
        matches!(err, LspError::Timeout { .. }),
        "the handshake error keeps its shape: {err:?}"
    );
    let tail = wait_for_tail(&c, "line 199");
    assert!(tail.captured);
    assert!(
        tail.text.contains("line 199"),
        "tail must end at the newest byte: {tail:?}"
    );
    assert!(
        !tail.text.contains("line 0\n"),
        "oldest output must be cut: {tail:?}"
    );
    assert!(
        tail.text.len() <= 64,
        "bound ignored: {} bytes",
        tail.text.len()
    );
    let written: u64 = (0..200).map(|i| format!("line {i}\n").len() as u64).sum();
    assert_eq!(
        tail.dropped_bytes + tail.text.len() as u64,
        written,
        "every byte is either kept or counted as dropped"
    );
    let shown = tail.to_string();
    assert!(
        shown.contains("earlier bytes dropped"),
        "cut must be reported: {shown}"
    );
    assert!(
        shown.contains("server stderr") && shown.contains("line 199"),
        "the printed tail must carry the kept bytes: {shown}"
    );
    let _ = c.close();
}

#[test]
fn stderr_tail_zero_keeps_nothing_but_counts() {
    let mut c = Client::new_stdio(&noisy_server(3)).expect("spawn sh");
    c.configure(ClientConfig {
        stderr_tail_bytes: 0,
        shutdown_timeout: Duration::from_millis(50),
        exit_deadline: Duration::from_millis(50),
        ..ClientConfig::default()
    });
    let until = Instant::now() + Duration::from_secs(5);
    while c.stderr_tail().dropped_bytes < 21 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    let tail = c.stderr_tail();
    assert_eq!(tail.text, "");
    assert_eq!(tail.dropped_bytes, 21, "3 lines of 7 bytes: {tail:?}");
    let _ = c.close();
}

#[test]
fn configure_round_trips_every_bound() {
    let cfg = ClientConfig {
        initialize_timeout: Duration::from_millis(1),
        call_timeout: Duration::from_millis(2),
        rename_deadline: Duration::from_millis(3),
        rename_retry: Duration::from_millis(4),
        query_deadline: Duration::from_millis(5),
        query_retry: Duration::from_millis(6),
        ready_deadline: Duration::from_millis(7),
        ready_retry: Duration::from_millis(8),
        shutdown_timeout: Duration::from_millis(9),
        exit_deadline: Duration::from_millis(10),
        exit_poll: Duration::from_millis(11),
        process_id: None,
        initialization_options: Some(
            serde_json::json!({"preferences": {"quotePreference": "single"}}),
        ),
        experimental_capabilities: Some(serde_json::json!({"serverStatusNotification": true})),
        ready_failures: vec![rust_analyzer_failed()],
        ready_signals: vec![
            rust_analyzer_quiescent(),
            gopls_finished_loading(ReadyMode::Once),
        ],
        diagnostic_failures: vec![DiagnosticFailure {
            when: serde_json::json!({"source": "go list"}),
            file_name: Some("go.mod".to_string()),
            mode: ReadyMode::Once,
        }],
        hover_content_format: vec![MarkupKind::PlainText],
        stderr_tail_bytes: 12,
    };
    let mut c = Client::new_stdio(&silent_server()).expect("spawn sleep");
    c.configure(cfg.clone());
    assert_eq!(c.config(), cfg);
}

#[test]
fn initialize_fails_at_the_declared_timeout() {
    let declared = Duration::from_millis(200);
    let mut c = Client::new_stdio(&silent_server()).expect("spawn sleep");
    c.configure(ClientConfig {
        initialize_timeout: declared,
        ..ClientConfig::default()
    });
    let started = Instant::now();
    let err = c
        .initialize(&lsp::file_uri("/tmp").to_string())
        .expect_err("a silent server cannot initialize");
    let took = started.elapsed();
    match err {
        LspError::Timeout { method, after } => {
            assert_eq!(method, "initialize");
            assert_eq!(after, declared, "the error must name the DECLARED bound");
        }
        other => panic!("want Timeout, got {other:?}"),
    }
    assert!(took >= declared, "fired early: {took:?}");
    assert!(
        took < Duration::from_secs(5),
        "declared bound ignored: {took:?}"
    );
}

#[test]
fn pool_initialize_fails_at_the_declared_timeout() {
    // The pool initializes right after its spawn, so the declaration must
    // travel with the pool — the caller never holds the client first.
    let declared = Duration::from_millis(200);
    let pool = LspPool::with_client_config(
        silent_server(),
        Duration::from_secs(60),
        1,
        ClientConfig {
            initialize_timeout: declared,
            shutdown_timeout: Duration::from_millis(50),
            exit_deadline: Duration::from_millis(50),
            ..ClientConfig::default()
        },
    );
    let started = Instant::now();
    let err = pool
        .with_client(Path::new("/tmp"), "rust", |_| Ok(()))
        .expect_err("a silent server cannot initialize");
    let took = started.elapsed();
    match err {
        LspError::Timeout { method, after } => {
            assert_eq!(method, "initialize");
            assert_eq!(after, declared);
        }
        other => panic!("want Timeout, got {other:?}"),
    }
    assert!(
        took < Duration::from_secs(5),
        "declared bound ignored: {took:?}"
    );
    assert!(pool.is_empty(), "a failed handshake must not hold a slot");
}

#[test]
fn close_honours_declared_shutdown_waits() {
    let shutdown = Duration::from_millis(150);
    let exit = Duration::from_millis(250);
    let mut c = Client::new_stdio(&silent_server()).expect("spawn sleep");
    c.configure(ClientConfig {
        shutdown_timeout: shutdown,
        exit_deadline: exit,
        exit_poll: Duration::from_millis(10),
        ..ClientConfig::default()
    });
    let pid = c.server_pid().expect("owned child");
    assert!(pid_alive(pid), "precondition: child alive before close");
    let started = Instant::now();
    let err = c
        .close()
        .expect_err("a silent server never acknowledges shutdown");
    let took = started.elapsed();
    match err {
        LspError::Timeout { method, after } => {
            assert_eq!(method, "shutdown");
            assert_eq!(after, shutdown);
        }
        other => panic!("want shutdown Timeout, got {other:?}"),
    }
    assert!(
        took >= shutdown + exit,
        "exit deadline not waited out: {took:?}"
    );
    // The compiled waits were 2 s + 3 s; anything near that ignored the declaration.
    assert!(
        took < Duration::from_secs(2),
        "declared waits ignored: {took:?}"
    );
    assert!(!pid_alive(pid), "child must be killed and reaped");
}

/// One real-server scenario: a file whose DISK bytes lack a symbol, opened with
/// buffer content that defines it, then a definition query from another file
/// that can only resolve through the opened buffer.
struct RealCase {
    name: &'static str,
    server: &'static [&'static str],
    /// The server's own "done loading", declared so the readiness gate waits
    /// for it. A probe alone is not enough: rust-analyzer resolves one against
    /// its first crate graph, then answers the same query with an empty
    /// default for as long as it is still reading files.
    declare: fn(ClientConfig) -> ClientConfig,
    disk: &'static [(&'static str, &'static str)],
    opened: (&'static str, &'static str),
    query_file: &'static str,
    query_line: u32,
    needle: &'static str,
}

const REAL_CASES: &[RealCase] = &[
    RealCase {
        name: "rust-analyzer",
        server: &["rust-analyzer"],
        declare: |cfg| ClientConfig {
            experimental_capabilities: Some(serde_json::json!({"serverStatusNotification": true})),
            ready_signals: vec![rust_analyzer_quiescent()],
            ..cfg
        },
        disk: &[
            (
                "Cargo.toml",
                "[package]\nname = \"t\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
            ),
            (
                "src/lib.rs",
                "pub mod util;\npub fn f() -> i32 { util::helper() }\n",
            ),
            ("src/util.rs", "// helper exists only in the open buffer\n"),
        ],
        opened: ("src/util.rs", "pub fn helper() -> i32 { 1 }\n"),
        query_file: "src/lib.rs",
        query_line: 1,
        needle: "helper",
    },
    RealCase {
        name: "gopls",
        server: &["gopls"],
        declare: |cfg| ClientConfig {
            ready_signals: vec![gopls_finished_loading(ReadyMode::Once)],
            ..cfg
        },
        disk: &[
            ("go.mod", "module example.com/t\n\ngo 1.21\n"),
            ("a.go", "package t\n\nfunc F() int { return helper() }\n"),
            ("b.go", "package t\n"),
        ],
        opened: ("b.go", "package t\n\nfunc helper() int { return 1 }\n"),
        query_file: "a.go",
        query_line: 2,
        needle: "helper",
    },
];

fn run_real_case(case: &RealCase) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    for (rel, content) in case.disk {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        fs::write(&p, content).expect("fixture");
    }
    let root_str = root.to_str().expect("utf-8 root");
    let argv: Vec<String> = case.server.iter().map(|s| s.to_string()).collect();
    let mut c = Client::new_stdio(&argv).unwrap_or_else(|e| panic!("{}: spawn: {e}", case.name));
    c.configure((case.declare)(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        ready_deadline: Duration::from_secs(120),
        ready_retry: Duration::from_millis(250),
        ..ClientConfig::default()
    }));
    c.initialize(&lsp::file_uri(root_str).to_string())
        .unwrap_or_else(|e| panic!("{}: initialize: {e}", case.name));

    let opened = root.join(case.opened.0);
    let opened_str = opened.to_str().expect("utf-8 path");
    c.did_open(opened_str, case.opened.1)
        .unwrap_or_else(|e| panic!("{}: did_open: {e}", case.name));

    let query = root.join(case.query_file);
    let query_str = query.to_str().expect("utf-8 path");
    let content = fs::read_to_string(&query).expect("read query file");
    let line_text = content
        .lines()
        .nth(case.query_line as usize)
        .expect("query line");
    let col = line_text.find(case.needle).expect("needle on line") as u32;

    c.await_ready(query_str, &content, case.query_line, col)
        .unwrap_or_else(|e| panic!("{}: the opened buffer never resolved: {e}", case.name));
    let locs = c
        .definition(query_str, &content, case.query_line, col)
        .unwrap_or_else(|e| panic!("{}: definition: {e}", case.name));
    assert!(
        locs.iter()
            .any(|l| Path::new(&l.path).ends_with(case.opened.0)),
        "{}: definition must land in the OPENED buffer's file, got {locs:?}",
        case.name
    );

    assert!(
        c.did_close(opened_str)
            .unwrap_or_else(|e| panic!("{}: did_close: {e}", case.name)),
        "{}: an open document must report closed",
        case.name
    );
    assert!(
        !c.did_close(opened_str)
            .unwrap_or_else(|e| panic!("{}: second did_close: {e}", case.name)),
        "{}: a closed document must not be closed twice",
        case.name
    );
    c.close()
        .unwrap_or_else(|e| panic!("{}: close: {e}", case.name));
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn did_open_buffer_is_what_a_real_server_resolves_against() {
    require_real_tier();
    for case in REAL_CASES {
        eprintln!("real-server case: {}", case.name);
        run_real_case(case);
    }
}

/// A workspace MEMBER copied without its workspace: cargo cannot load it, so
/// rust-analyzer answers every query empty and never becomes ready.
const ORPHAN_MEMBER: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"member\"\nversion.workspace = true\nedition = \"2021\"\n",
    ),
    (
        "src/lib.rs",
        "pub fn helper() -> i32 { 1 }\npub fn f() -> i32 { helper() }\n",
    ),
];

/// [`ORPHAN_MEMBER`] as a crate of its own, which cargo loads.
const LOADABLE_CRATE: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    ),
    ORPHAN_MEMBER[1],
];

fn await_orphan_member(cfg: ClientConfig) -> (Result<(), LspError>, Duration) {
    await_rust_crate(ORPHAN_MEMBER, cfg)
}

/// Starts rust-analyzer on `files` under `cfg` and gates on a call that a
/// loaded workspace would resolve.
fn await_rust_crate(files: &[(&str, &str)], cfg: ClientConfig) -> (Result<(), LspError>, Duration) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    for (rel, content) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        fs::write(&p, content).expect("fixture");
    }
    let mut c = Client::new_stdio(&["rust-analyzer".to_string()]).expect("spawn rust-analyzer");
    c.configure(cfg);
    c.initialize(&lsp::file_uri(root.to_str().expect("utf-8")).to_string())
        .expect("initialize");
    let lib = root.join("src/lib.rs");
    // Wall clock, to line up against the timestamps in the server's own log.
    eprintln!(
        "await_ready starts at unix {:?}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
    );
    let started = Instant::now();
    let got = c.await_ready(
        lib.to_str().expect("utf-8"),
        files[1].1,
        1,
        "pub fn f() -> i32 { ".len() as u32,
    );
    let took = started.elapsed();
    let _ = c.close();
    (got, took)
}

/// The printed error, not only the field, must show the server's stderr: a
/// caller that logs the error is the reader it exists for.
fn assert_prints_stderr(err: &LspError, stderr: &lsp::StderrTail) {
    let shown = err.to_string();
    let newest = stderr
        .text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .expect("the server wrote to stderr");
    assert!(
        shown.contains("server stderr") && shown.contains(newest.trim_end()),
        "printed error must carry the stderr tail ending {newest:?}: {shown}"
    );
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn rust_analyzer_reported_failure_stops_readiness_at_once() {
    require_real_tier();
    let declared = ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(30),
        ready_deadline: Duration::from_secs(120),
        ready_retry: Duration::from_millis(250),
        experimental_capabilities: Some(serde_json::json!({"serverStatusNotification": true})),
        ready_failures: vec![rust_analyzer_failed()],
        ..ClientConfig::default()
    };
    let (got, took) = await_orphan_member(declared.clone());
    match got {
        Err(LspError::ServerReported {
            ref method,
            ref message,
            ref stderr,
            ..
        }) => {
            assert_eq!(method, "experimental/serverStatus");
            assert!(
                message.contains("Failed to load workspaces"),
                "server message must reach the caller: {message}"
            );
            assert!(stderr.captured);
            assert!(
                stderr.text.len() <= declared.stderr_tail_bytes,
                "stderr bound ignored"
            );
            assert_prints_stderr(got.as_ref().unwrap_err(), stderr);
            eprintln!("reported in {took:?}: {}", got.as_ref().unwrap_err());
        }
        other => panic!("want ServerReported, got {other:?}"),
    }
    // The server's own workspace load bounds how soon it can report, and that
    // stretches with machine load; the deadline is what must not be spent.
    assert!(
        took < declared.ready_deadline,
        "a reported failure must end the wait, not the deadline: {took:?}"
    );

    // Undeclared: probing alone decides, and the deadline
    // error carries what the server said on stderr.
    let (got, took) = await_orphan_member(ClientConfig {
        ready_deadline: Duration::from_secs(8),
        experimental_capabilities: None,
        ready_failures: Vec::new(),
        ..declared.clone()
    });
    match got {
        Err(LspError::ReadyProbeDeadline { ref stderr, .. }) => {
            assert!(stderr.captured);
            assert!(
                stderr.text.contains("workspace"),
                "the server's own explanation must ride on the error: {stderr:?}"
            );
            assert_prints_stderr(got.as_ref().unwrap_err(), stderr);
        }
        other => panic!("want ReadyProbeDeadline, got {other:?}"),
    }
    assert!(
        took >= Duration::from_secs(8),
        "undeclared must wait out the deadline: {took:?}"
    );
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn rust_analyzer_quiescent_signal_gates_the_probe() {
    require_real_tier();
    let declared = ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(30),
        ready_deadline: Duration::from_secs(120),
        ready_retry: Duration::from_millis(250),
        experimental_capabilities: Some(serde_json::json!({"serverStatusNotification": true})),
        ready_signals: vec![rust_analyzer_quiescent()],
        ..ClientConfig::default()
    };
    let (got, took) = await_rust_crate(LOADABLE_CRATE, declared.clone());
    assert!(got.is_ok(), "a loaded crate must pass the signal: {got:?}");
    eprintln!("quiescent and resolved in {took:?}");

    // Control: the signal declared, but the server never asked to send it.
    let (got, _) = await_rust_crate(
        LOADABLE_CRATE,
        ClientConfig {
            ready_deadline: Duration::from_secs(5),
            experimental_capabilities: None,
            ..declared
        },
    );
    match got {
        Err(LspError::ReadySignalDeadline {
            ref signal,
            ref latest,
            ..
        }) => {
            assert_eq!(**signal, rust_analyzer_quiescent());
            assert_eq!(*latest, None, "the server never sent the method at all");
        }
        other => panic!("want ReadySignalDeadline naming the signal, got {other:?}"),
    }
}

/// A Go module that loads: `a.go` calls a function `b.go` defines.
const HEALTHY_GO_MODULE: &[(&str, &str)] = &[
    ("go.mod", "module example.com/t\n\ngo 1.21\n"),
    ("a.go", "package t\n\nfunc F() int { return helper() }\n"),
    ("b.go", "package t\n\nfunc helper() int { return 1 }\n"),
];

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn gopls_once_signal_outlives_the_messages_that_follow_it() {
    require_real_tier();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    for (rel, content) in HEALTHY_GO_MODULE {
        fs::write(root.join(rel), content).expect("fixture");
    }
    // gopls's own setting: it announces each round of work, and to a client
    // that declares no work-done progress it does so on `window/showMessage`,
    // the method its "Finished loading packages." came on. The module is
    // healthy throughout; only the server's talk is turned up.
    let declared = |ready_signals: Vec<ReadySignal>, ready_deadline: Duration| ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        ready_deadline,
        ready_retry: Duration::from_millis(250),
        initialization_options: Some(serde_json::json!({"verboseWorkDoneProgress": true})),
        ready_signals,
        ..ClientConfig::default()
    };
    let once = gopls_finished_loading(ReadyMode::Once);
    let (a_go, content) = (root.join("a.go"), HEALTHY_GO_MODULE[1].1);
    let col = content.lines().nth(2).expect("line 2").find("helper");
    let col = col.expect("the call") as u32;
    let gate = |c: &mut Client| c.await_ready(a_go.to_str().expect("utf-8"), content, 2, col);

    let mut c = Client::new_stdio(&["gopls".to_string()]).expect("spawn gopls");
    c.configure(declared(vec![once.clone()], Duration::from_secs(120)));
    c.initialize(&lsp::file_uri(root.to_str().expect("utf-8")).to_string())
        .expect("initialize");
    gate(&mut c).unwrap_or_else(|e| panic!("a loaded module must pass its once signal: {e}"));

    // The same server must now ALSO have said "Done." last. Passing proves
    // both halves in one read: its latest showMessage is no longer the load
    // announcement, and the once signal holds regardless.
    let done_is_latest = ReadySignal {
        method: "window/showMessage".to_string(),
        when: serde_json::json!({"message": "Done."}),
        mode: ReadyMode::Latest,
    };
    c.configure(declared(
        vec![once, done_is_latest],
        Duration::from_secs(120),
    ));
    gate(&mut c).unwrap_or_else(|e| panic!("a later showMessage un-met the once signal: {e}"));

    // Control, same server, same messages, read as a state: never met again.
    let as_state = gopls_finished_loading(ReadyMode::Latest);
    c.configure(declared(vec![as_state.clone()], Duration::from_secs(3)));
    match gate(&mut c) {
        Err(LspError::ReadySignalDeadline {
            ref signal,
            latest: Some(ref said),
            ..
        }) => {
            assert_eq!(**signal, as_state);
            assert_ne!(said["message"], "Finished loading packages.");
            eprintln!("read as a state, gopls last said: {said}");
        }
        other => panic!("want ReadySignalDeadline carrying a later message, got {other:?}"),
    }
    c.close().expect("close");
}

/// Writes `files` under a fresh root and starts `server` on it under `cfg`.
/// The directory guard is returned so the root outlives the server.
fn start_server(
    server: &str,
    files: &[(&str, &str)],
    cfg: ClientConfig,
) -> (tempfile::TempDir, std::path::PathBuf, Client) {
    start_argv(&[server], files, cfg)
}

/// [`start_server`] for a server that takes arguments.
fn start_argv(
    argv: &[&str],
    files: &[(&str, &str)],
    cfg: ClientConfig,
) -> (tempfile::TempDir, std::path::PathBuf, Client) {
    let server = argv[0];
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    for (rel, content) in files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        fs::write(&p, content).expect("fixture");
    }
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    let mut c = Client::new_stdio(&argv).unwrap_or_else(|e| panic!("spawn {server}: {e}"));
    c.configure(cfg);
    c.initialize(&lsp::file_uri(root.to_str().expect("utf-8")).to_string())
        .unwrap_or_else(|e| panic!("{server}: initialize: {e}"));
    (dir, root, c)
}

/// [`HEALTHY_GO_MODULE`] with a `go.mod` whose `require (` block never
/// closes: the module cannot load, and gopls says where.
const MALFORMED_GO_MODULE: &[(&str, &str)] = &[
    ("go.mod", "module example.com/t\n\ngo 1.21\n\nrequire (\n"),
    HEALTHY_GO_MODULE[1],
    HEALTHY_GO_MODULE[2],
];

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn gopls_malformed_go_mod_is_reported_where_it_is() {
    require_real_tier();
    let declared = |diagnostic_failures: Vec<DiagnosticFailure>, ready_deadline| ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        ready_deadline,
        ready_retry: Duration::from_millis(250),
        diagnostic_failures,
        ..ClientConfig::default()
    };
    let in_go_mod = DiagnosticFailure {
        when: serde_json::json!({"severity": 1}),
        file_name: Some("go.mod".to_string()),
        mode: ReadyMode::Once,
    };
    let from_go_list = DiagnosticFailure {
        when: serde_json::json!({"source": "go list"}),
        file_name: None,
        mode: ReadyMode::Once,
    };
    let content = MALFORMED_GO_MODULE[1].1;
    let col = content.lines().nth(2).expect("line 2").find("helper");
    let col = col.expect("the call") as u32;

    let deadline = Duration::from_secs(120);
    let (_dir, root, mut c) = start_server(
        "gopls",
        MALFORMED_GO_MODULE,
        declared(vec![in_go_mod.clone()], deadline),
    );
    let a_go = root.join("a.go");
    let a_go = a_go.to_str().expect("utf-8");
    let started = Instant::now();
    let got = c.await_ready(a_go, content, 2, col);
    let took = started.elapsed();
    match got {
        Err(LspError::ServerReported {
            ref method,
            ref message,
            ref locations,
            ..
        }) => {
            assert_eq!(method, lsp::PUBLISH_DIAGNOSTICS_METHOD);
            assert_eq!(
                locations.len(),
                1,
                "one rule, one diagnostic: {locations:?}"
            );
            let at = &locations[0];
            assert_eq!(at.path, "go.mod", "named relative to the root");
            assert_eq!(
                at.uri,
                lsp::file_uri(root.join("go.mod").to_str().expect("utf-8")).to_string()
            );
            assert_eq!(
                (at.start_line, at.start_char, at.end_line, at.end_char),
                (5, 0, 5, 0),
                "where the unclosed block runs out"
            );
            assert_eq!(at.severity, Some(Severity::Error));
            assert_eq!(at.source.as_deref(), Some("syntax"));
            assert_eq!(at.code, None);
            assert!(
                at.message.contains("unterminated block"),
                "the server's own words: {}",
                at.message
            );
            assert_eq!(message, &at.message);
            let shown = got.as_ref().unwrap_err().to_string();
            let line = format!("\n  at go.mod:6:1: {} [syntax]", at.message);
            assert!(shown.contains(&line), "printed without its place: {shown}");
            eprintln!("reported in {took:?}: {shown}");
        }
        other => panic!("want a located ServerReported, got {other:?}"),
    }
    assert!(
        took < deadline,
        "a reported failure must end the wait, not the deadline: {took:?}"
    );

    // gopls goes on to say more: a second diagnostic, on the opened source
    // file, that the module never initialised. It comes on gopls's own
    // schedule, so it is declared now and asked for after the gate, and is
    // found whichever side of this line it arrived on.
    c.configure(declared(vec![in_go_mod, from_go_list], deadline));
    let until = Instant::now() + Duration::from_secs(60);
    let located = loop {
        match c.check_failures() {
            Err(LspError::ServerReported { locations, .. }) if locations.len() == 2 => {
                break locations;
            }
            Err(LspError::ServerReported { .. }) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(100));
            }
            other => panic!("the go list diagnostic never arrived: {other:?}"),
        }
    };
    let (in_source, in_manifest) = (&located[0], &located[1]);
    assert_eq!(in_manifest.path, "go.mod");
    assert_eq!(in_source.path, "a.go");
    assert_eq!(in_source.source.as_deref(), Some("go list"));
    assert!(
        in_source.message.contains("go.mod"),
        "gopls names the manifest in the source file's diagnostic: {}",
        in_source.message
    );
    eprintln!(
        "then, {:?} after the gate: {in_source:?}",
        started.elapsed() - took
    );
    c.close().expect("close");

    // Control: the same module with no diagnostic declared. gopls refuses
    // every probe, so the gate waits out its deadline and can say only that;
    // the diagnostic is still there to be read.
    let (_dir, root, mut c) = start_server(
        "gopls",
        MALFORMED_GO_MODULE,
        declared(Vec::new(), Duration::from_secs(5)),
    );
    let a_go = root.join("a.go");
    match c.await_ready(a_go.to_str().expect("utf-8"), content, 2, col) {
        Err(LspError::ReadyProbeDeadline {
            last: ProbeAnswer::Refused { ref message },
            line,
            character,
            ..
        }) => {
            assert_eq!((line, character), (2, col));
            eprintln!("undeclared, the last probe was refused: {message}");
        }
        other => panic!("want ReadyProbeDeadline on a refused probe, got {other:?}"),
    }
    let published = c.published_diagnostics();
    assert!(
        published
            .iter()
            .any(|d| d.path == "go.mod" && d.severity == Some(Severity::Error)),
        "read without any rule declared: {published:?}"
    );
    c.check_failures()
        .expect("no rule declared, so no diagnostic is a failure");
    c.close().expect("close");
}

/// A crate whose manifest does not parse: `[package` never closes.
const MALFORMED_CARGO_TOML: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    ),
    ORPHAN_MEMBER[1],
];

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn rust_analyzer_locates_a_malformed_cargo_toml_only_on_stderr() {
    require_real_tier();
    let deadline = Duration::from_secs(120);
    // Every diagnostic on every document is declared a failure, so an empty
    // `locations` below means the server published none at all.
    let any_diagnostic = DiagnosticFailure {
        when: serde_json::json!({}),
        file_name: None,
        mode: ReadyMode::Once,
    };
    let (_dir, root, mut c) = start_server(
        "rust-analyzer",
        MALFORMED_CARGO_TOML,
        ClientConfig {
            initialize_timeout: Duration::from_secs(60),
            call_timeout: Duration::from_secs(30),
            ready_deadline: deadline,
            ready_retry: Duration::from_millis(250),
            experimental_capabilities: Some(serde_json::json!({"serverStatusNotification": true})),
            ready_failures: vec![rust_analyzer_failed()],
            diagnostic_failures: vec![any_diagnostic],
            ..ClientConfig::default()
        },
    );
    let lib = root.join("src/lib.rs");
    let started = Instant::now();
    let got = c.await_ready(
        lib.to_str().expect("utf-8"),
        MALFORMED_CARGO_TOML[1].1,
        1,
        "pub fn f() -> i32 { ".len() as u32,
    );
    let took = started.elapsed();
    match got {
        Err(LspError::ServerReported {
            ref method,
            ref message,
            ref locations,
            ref stderr,
        }) => {
            assert_eq!(method, "experimental/serverStatus");
            assert_eq!(message, "Failed to load workspaces.");
            assert!(
                locations.is_empty(),
                "rust-analyzer now publishes a diagnostic for the manifest: {locations:?}"
            );
            // The place is cargo's own rendering, passed through on stderr:
            // line 1, column 9, where `[package` should have closed.
            assert!(stderr.captured);
            assert!(
                stderr.text.contains("Cargo.toml:1:9"),
                "the manifest position must ride on the error: {stderr:?}"
            );
            assert_prints_stderr(got.as_ref().unwrap_err(), stderr);
            eprintln!("reported in {took:?}: {}", got.as_ref().unwrap_err());
        }
        other => panic!("want ServerReported, got {other:?}"),
    }
    assert!(
        took < deadline,
        "a reported failure must end the wait, not the deadline: {took:?}"
    );
    assert_eq!(c.published_diagnostics(), Vec::new());
    c.close().expect("close");
}

/// One symbol to hover: the zero-based line, the text on it that names the
/// symbol, and what the server's answer must contain.
struct HoverSymbol {
    what: &'static str,
    line: u32,
    needle: &'static str,
    contains: &'static [&'static str],
}

/// One real server asked for the documentation of symbols defined OUTSIDE
/// the project it was started on.
struct HoverCase {
    argv: &'static [&'static str],
    files: &'static [(&'static str, &'static str)],
    /// The server's own readiness declarations, as a caller makes them.
    declare: fn(ClientConfig) -> ClientConfig,
    file: &'static str,
    /// A reference that resolves once the project has loaded.
    ready: (u32, &'static str),
    symbols: &'static [HoverSymbol],
    /// A zero-based (line, character) with no symbol under it.
    nothing: (u32, u32),
}

/// The UTF-16 column of `needle` on `line`; the fixtures are ASCII, so its
/// byte offset.
fn column_of(content: &str, line: u32, needle: &str) -> u32 {
    let text = content.lines().nth(line as usize).expect("line");
    text.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not on line {line}: {text:?}")) as u32
}

/// The two halves of the hover contract against one real server. Declaring
/// markdown first, every symbol answers in markdown with its signature and
/// its documentation. Declaring plain text alone, the same server answers in
/// plain text: the format is the caller's declaration, read back from the
/// server rather than assumed.
fn hover_proof(case: &HoverCase) {
    let server = case.argv[0];
    let shown = hover_case(case, &ClientConfig::default().hover_content_format);
    for (symbol, hover) in case.symbols.iter().zip(&shown) {
        assert_eq!(
            hover.kind,
            MarkupKind::Markdown,
            "{server}: {}",
            symbol.what
        );
        for want in symbol.contains {
            assert!(
                hover.text.contains(want),
                "{server}: {} must show {want:?}, got: {}",
                symbol.what,
                hover.text
            );
        }
    }
    let plain = hover_case(case, &[MarkupKind::PlainText]);
    for (symbol, hover) in case.symbols.iter().zip(&plain) {
        assert_eq!(
            hover.kind,
            MarkupKind::PlainText,
            "{server}: {}",
            symbol.what
        );
        assert!(
            !hover.text.is_empty() && !hover.text.contains("```"),
            "{server}: {} as plain text carries no code fence: {}",
            symbol.what,
            hover.text
        );
    }
}

/// Runs `case` with `formats` declared and returns each symbol's hover, in
/// order, having checked each one's span and the position with nothing under
/// it.
fn hover_case(case: &HoverCase, formats: &[MarkupKind]) -> Vec<lsp::Hover> {
    let server = case.argv[0];
    let cfg = (case.declare)(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        ready_deadline: Duration::from_secs(180),
        ready_retry: Duration::from_millis(250),
        hover_content_format: formats.to_vec(),
        ..ClientConfig::default()
    });
    let (_dir, root, mut c) = start_argv(case.argv, case.files, cfg);
    let path = root.join(case.file);
    let path = path.to_str().expect("utf-8");
    let content = hover_content(case);
    let (ready_line, ready_needle) = case.ready;
    let ready_col = column_of(content, ready_line, ready_needle);
    c.await_ready(path, content, ready_line, ready_col)
        .unwrap_or_else(|e| panic!("{server}: never ready: {e}"));

    let mut got = Vec::new();
    for symbol in case.symbols {
        // One character into the name: on the symbol, not at its edge.
        let col = column_of(content, symbol.line, symbol.needle) + 1;
        let hover = c
            .hover(path, content, symbol.line, col)
            .unwrap_or_else(|e| panic!("{server}: hover on {}: {e}", symbol.what))
            .unwrap_or_else(|| panic!("{server}: nothing shown for {}", symbol.what));
        // Where the server read it from: the files that must be installed
        // for the hover to carry documentation.
        let defined_in: Vec<String> = c
            .definition(path, content, symbol.line, col)
            .unwrap_or_else(|e| panic!("{server}: definition of {}: {e}", symbol.what))
            .into_iter()
            .map(|l| l.path)
            .collect();
        eprintln!(
            "{server} {formats:?} {} ({:?}, defined in {defined_in:?}):\n{}\n",
            symbol.what, hover.kind, hover.text
        );
        let name = col - 1;
        assert_eq!(
            hover.range,
            Some(lsp::SymbolLocation {
                path: path.to_string(),
                start_line: symbol.line,
                start_char: name,
                end_line: symbol.line,
                end_char: name + symbol.needle.len() as u32,
            }),
            "{server}: {} is the span hovered",
            symbol.what
        );
        got.push(hover);
    }
    let (line, character) = case.nothing;
    assert_eq!(
        c.hover(path, content, line, character)
            .unwrap_or_else(|e| panic!("{server}: hover on nothing is not an error: {e}")),
        None,
        "{server}: nothing is under {line}:{character}"
    );
    // Not this test's subject, and tsc ends its connection without answering
    // `shutdown` some of the time.
    if let Err(e) = c.close() {
        eprintln!("{server}: close: {e}");
    }
    got
}

/// The real-server cases are `#[ignore]`d, so reaching one means the operator
/// selected it on purpose; without the opt-in that is a mistake to see, not
/// one to swallow into a green result.
fn require_real_tier() {
    assert!(
        std::env::var("BAGE_LSP_REAL_TEST").ok().as_deref() == Some("1"),
        "the real-server tier needs BAGE_LSP_REAL_TEST=1 (and its servers on PATH)"
    );
}

/// A crate depending on a registry crate that is served from a vendored
/// directory, so cargo resolves it with no network.
const HOVER_RUST: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nthirdparty = \"1.0.0\"\n",
    ),
    (
        ".cargo/config.toml",
        "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"vendor\"\n",
    ),
    (
        "vendor/thirdparty/Cargo.toml",
        "[package]\nname = \"thirdparty\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    ),
    (
        "vendor/thirdparty/.cargo-checksum.json",
        "{\"files\":{},\"package\":\"0000000000000000000000000000000000000000000000000000000000000000\"}",
    ),
    (
        "vendor/thirdparty/src/lib.rs",
        "//! A vendored third-party crate.\n\n/// Doubles `n`, the documented third-party way.\n///\n/// # Examples\n///\n/// ```\n/// assert_eq!(thirdparty::documented(2), 4);\n/// ```\npub fn documented(n: usize) -> usize {\n    n * 2\n}\n",
    ),
    (
        "src/lib.rs",
        "use std::collections::HashMap;\n\npub fn f() -> usize {\n    let mut m: HashMap<String, i32> = HashMap::new();\n    m.insert(String::from(\"a\"), 1);\n    thirdparty::documented(m.len())\n}\n",
    ),
];

fn rust_hover_case() -> HoverCase {
    HoverCase {
        argv: &["rust-analyzer"],
        files: HOVER_RUST,
        declare: |cfg| ClientConfig {
            experimental_capabilities: Some(serde_json::json!({"serverStatusNotification": true})),
            ready_failures: vec![rust_analyzer_failed()],
            ready_signals: vec![rust_analyzer_quiescent()],
            ..cfg
        },
        file: "src/lib.rs",
        ready: (5, "documented"),
        symbols: &[
            HoverSymbol {
                what: "std HashMap::insert",
                line: 4,
                needle: "insert",
                contains: &[
                    "pub fn insert(&mut self, k: K, v: V) -> Option<V>",
                    "Inserts a key-value pair into the map.",
                ],
            },
            HoverSymbol {
                what: "third-party documented",
                line: 5,
                needle: "documented",
                contains: &[
                    "pub fn documented(n: usize) -> usize",
                    "Doubles `n`, the documented third-party way.",
                ],
            },
        ],
        nothing: (1, 0),
    }
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn rust_analyzer_hover_shows_std_and_dependency_docs() {
    require_real_tier();
    hover_proof(&rust_hover_case());
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn rust_analyzer_hover_needs_the_standard_library_sources() {
    require_real_tier();
    // rust-analyzer's own setting for where the standard library's sources
    // are, pointed at a directory holding none: a toolchain installed
    // without its `rust-src` component.
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().canonicalize().expect("canonical dir");
    let case = rust_hover_case();
    let cfg = (case.declare)(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        ready_deadline: Duration::from_secs(180),
        ready_retry: Duration::from_millis(250),
        initialization_options: Some(serde_json::json!({"cargo": {"sysrootSrc": absent}})),
        ..ClientConfig::default()
    });
    let (_dir, root, mut c) = start_argv(case.argv, case.files, cfg);
    let path = root.join(case.file);
    let path = path.to_str().expect("utf-8");
    let content = HOVER_RUST[5].1;
    let ready = column_of(content, case.ready.0, case.ready.1);
    c.await_ready(path, content, case.ready.0, ready)
        .unwrap_or_else(|e| panic!("the dependency still resolves: {e}"));
    let [std_symbol, dependency] = case.symbols else {
        panic!("two symbols");
    };
    let hover_at = |c: &mut Client, symbol: &HoverSymbol| {
        let col = column_of(content, symbol.line, symbol.needle) + 1;
        c.hover(path, content, symbol.line, col)
            .unwrap_or_else(|e| panic!("hover on {}: {e}", symbol.what))
    };
    let from_std = hover_at(&mut c, std_symbol);
    eprintln!("without the sources, {}: {from_std:?}", std_symbol.what);
    assert_eq!(
        from_std, None,
        "with no sources the std symbol is unresolved: nothing to show, not a bare signature"
    );
    let from_dependency = hover_at(&mut c, dependency).expect("the dependency's hover");
    for want in dependency.contains {
        assert!(
            from_dependency.text.contains(want),
            "a dependency's own sources are unaffected: {}",
            from_dependency.text
        );
    }
    if let Err(e) = c.close() {
        eprintln!("rust-analyzer: close: {e}");
    }
}

const HOVER_GO: &[(&str, &str)] = &[
    ("go.mod", "module example.com/t\n\ngo 1.21\n"),
    (
        "a.go",
        "package t\n\nimport \"fmt\"\n\n// F prints.\nfunc F() int {\n\tfmt.Println(\"x\")\n\treturn 1\n}\n",
    ),
];

fn go_hover_case() -> HoverCase {
    HoverCase {
        argv: &["gopls"],
        files: HOVER_GO,
        declare: |cfg| ClientConfig {
            ready_signals: vec![gopls_finished_loading(ReadyMode::Once)],
            ..cfg
        },
        file: "a.go",
        ready: (6, "Println"),
        symbols: &[HoverSymbol {
            what: "stdlib fmt.Println",
            line: 6,
            needle: "Println",
            contains: &[
                "func fmt.Println(a ...any) (n int, err error)",
                "Println formats using the default formats for its operands",
            ],
        }],
        nothing: (3, 0),
    }
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn gopls_hover_shows_stdlib_docs() {
    require_real_tier();
    hover_proof(&go_hover_case());
}

/// The query deadline the out-of-text tests declare: long enough for several
/// retries, short enough that every refused query can wait it out in full.
const SHORT_QUERY_DEADLINE: Duration = Duration::from_secs(1);

type Ask = fn(&mut Client, &str, &str, u32, u32) -> Result<String, LspError>;

/// Hover and definition, each answering with what it found, as text.
const POSITION_QUERIES: [(&str, Ask); 2] = [
    ("textDocument/hover", |c, path, content, line, character| {
        c.hover(path, content, line, character)
            .map(|h| format!("{h:?}"))
    }),
    (
        "textDocument/definition",
        |c, path, content, line, character| {
            c.definition(path, content, line, character)
                .map(|l| format!("{l:?}"))
        },
    ),
];

/// Starts `case`'s server under [`SHORT_QUERY_DEADLINE`] and asks hover and
/// definition at each position of `outside`, which lies outside the text and
/// which the server refuses in words containing the given fragment. A
/// refusal is what a server still loading sends too, so bage retries it and
/// the caller gets `QueryDeadline` after the whole deadline, the refusal as
/// its `last`. Returns the client, still usable, for the caller's own checks.
fn refused_outside_the_text(
    case: &HoverCase,
    outside: &[(&str, (u32, u32), &str)],
) -> (tempfile::TempDir, String, Client) {
    let server = case.argv[0];
    let cfg = (case.declare)(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        ready_deadline: Duration::from_secs(180),
        ready_retry: Duration::from_millis(250),
        query_deadline: SHORT_QUERY_DEADLINE,
        query_retry: Duration::from_millis(100),
        ..ClientConfig::default()
    });
    let (dir, root, mut c) = start_argv(case.argv, case.files, cfg);
    let path = root.join(case.file);
    let path = path.to_str().expect("utf-8").to_string();
    let content = hover_content(case);
    let ready = column_of(content, case.ready.0, case.ready.1);
    c.await_ready(&path, content, case.ready.0, ready)
        .unwrap_or_else(|e| panic!("{server}: never ready: {e}"));

    for (what, (line, character), says) in outside {
        for (method, ask) in POSITION_QUERIES {
            let start = Instant::now();
            let got = ask(&mut c, &path, content, *line, *character);
            let took = start.elapsed();
            match got {
                Err(LspError::QueryDeadline {
                    method: asked,
                    path: of,
                    after,
                    last,
                }) => {
                    assert_eq!(asked, method, "{server}: {what}");
                    assert_eq!(of, path, "{server}: {what}");
                    assert_eq!(after, SHORT_QUERY_DEADLINE, "{server}: {what}");
                    assert!(
                        last.contains(says),
                        "{server}: {method} {what}: the server's own refusal is kept: {last}"
                    );
                    eprintln!("{server} {method} {what} after {took:?}: {last}");
                }
                other => panic!("{server}: {method} {what}: want QueryDeadline, got {other:?}"),
            }
            assert!(
                took >= SHORT_QUERY_DEADLINE,
                "{server}: {method} {what}: retried for the whole deadline, took {took:?}"
            );
        }
    }
    (dir, path, c)
}

/// The text of the file `case` queries.
fn hover_content(case: &HoverCase) -> &'static str {
    case.files
        .iter()
        .find(|(rel, _)| *rel == case.file)
        .expect("the queried file is a fixture")
        .1
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn gopls_position_outside_the_text_is_retried_to_the_query_deadline() {
    require_real_tier();
    let case = go_hover_case();
    let (_dir, path, mut c) = refused_outside_the_text(
        &case,
        &[
            (
                "a column past the end of its line",
                (6, 500),
                "column is beyond end of line",
            ),
            (
                "a line past the end of the file",
                (500, 0),
                "line number 500 out of range",
            ),
        ],
    );
    // The refusals cost time and nothing else: the session still answers.
    let symbol = &case.symbols[0];
    let content = hover_content(&case);
    let col = column_of(content, symbol.line, symbol.needle) + 1;
    let hover = c.hover(&path, content, symbol.line, col).expect("hover");
    assert!(hover.is_some(), "gopls: a real position still answers");
    c.close().expect("close");
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn rust_analyzer_line_outside_the_text_is_retried_to_the_query_deadline() {
    require_real_tier();
    let case = rust_hover_case();
    let (_dir, path, mut c) = refused_outside_the_text(
        &case,
        &[(
            "a line past the end of the file",
            (500, 0),
            "Invalid offset LineCol { line: 500, col: 0 }",
        )],
    );
    // Which positions a server refuses is the server's: this one ANSWERS a
    // column past the end of its line, with nothing, and no deadline is spent.
    let content = hover_content(&case);
    let start = Instant::now();
    let hover = c.hover(&path, content, 4, 500);
    assert!(
        matches!(hover, Ok(None)),
        "answered, not refused: {hover:?}"
    );
    let defined = c.definition(&path, content, 4, 500);
    assert!(
        matches!(defined, Ok(ref found) if found.is_empty()),
        "answered, not refused: {defined:?}"
    );
    assert!(
        start.elapsed() < SHORT_QUERY_DEADLINE,
        "an answer is final and is not retried: {:?}",
        start.elapsed()
    );
    if let Err(e) = c.close() {
        eprintln!("rust-analyzer: close: {e}");
    }
}

const HOVER_TS: &[(&str, &str)] = &[
    (
        "tsconfig.json",
        "{\n  \"compilerOptions\": {\"strict\": true, \"target\": \"es2022\", \"module\": \"esnext\", \"moduleResolution\": \"bundler\"},\n  \"include\": [\"*.ts\"]\n}\n",
    ),
    (
        "package.json",
        "{\"name\": \"t\", \"version\": \"0.0.0\", \"type\": \"module\"}\n",
    ),
    (
        "node_modules/thirdparty/package.json",
        "{\"name\": \"thirdparty\", \"version\": \"1.0.0\", \"main\": \"index.js\", \"types\": \"index.d.ts\"}\n",
    ),
    (
        "node_modules/thirdparty/index.js",
        "export function documented(n) { return n * 2; }\n",
    ),
    (
        "node_modules/thirdparty/index.d.ts",
        "/**\n * Doubles `n`, the documented third-party way.\n *\n * @param n the number to double\n * @returns twice `n`\n */\nexport declare function documented(n: number): number;\n",
    ),
    (
        "a.ts",
        "import { documented } from \"thirdparty\";\n\nexport function f(): number {\n  const n = parseInt(\"42\", 10);\n  return documented(Math.max(n, 1));\n}\n",
    ),
];

fn ts_hover_case() -> HoverCase {
    HoverCase {
        argv: &["tsc", "--lsp", "--stdio"],
        files: HOVER_TS,
        declare: |cfg| cfg,
        file: "a.ts",
        ready: (4, "documented"),
        symbols: &[
            HoverSymbol {
                what: "lib parseInt",
                line: 3,
                needle: "parseInt",
                contains: &[
                    "function parseInt(string: string, radix?: number): number",
                    "Converts a string to an integer.",
                ],
            },
            HoverSymbol {
                what: "node_modules documented",
                line: 4,
                needle: "documented",
                contains: &[
                    "function documented(n: number): number",
                    "Doubles `n`, the documented third-party way.",
                ],
            },
        ],
        nothing: (1, 0),
    }
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn tsc_hover_shows_lib_and_node_modules_docs() {
    require_real_tier();
    hover_proof(&ts_hover_case());
}

const HOVER_PY: &[(&str, &str)] = &[
    ("pyrightconfig.json", "{\n  \"include\": [\".\"]\n}\n"),
    (
        "a.py",
        "import json\nimport os.path\n\n\ndef f() -> str:\n    data = json.dumps({\"a\": 1})\n    return os.path.join(\"a\", data)\n",
    ),
];

fn py_hover_case() -> HoverCase {
    HoverCase {
        argv: &["pyright-langserver", "--stdio"],
        files: HOVER_PY,
        declare: |cfg| cfg,
        file: "a.py",
        ready: (5, "dumps"),
        symbols: &[HoverSymbol {
            what: "stdlib json.dumps",
            line: 5,
            needle: "dumps",
            // The signature is from the stubs pyright ships. The prose is
            // not: stubs carry no docstrings, so pyright reads it from the
            // standard library of a Python interpreter on PATH, and shows
            // the signature alone when it finds none.
            contains: &["def dumps(", "Serialize `obj` to a JSON formatted `str`."],
        }],
        nothing: (2, 0),
    }
}

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn pyright_hover_shows_stdlib_docs() {
    require_real_tier();
    hover_proof(&py_hover_case());
}

/// The opening of Hylla's `crates/hylla-agent-skill/src/lib.rs`, byte for byte
/// (`use std::fmt::Write as _;`, then `SkillTool<'_>`), the file whose `'_`
/// once became a readiness probe rust-analyzer can never resolve.
const AGENT_SKILL_HEADER: &str = include_str!("../testdata/readiness/agent_skill_header.rs");

#[test]
#[ignore = "real language servers: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_session -- --ignored"]
fn rust_analyzer_is_ready_by_a_cross_file_probe_and_never_by_blank_or_in_file_ones() {
    require_real_tier();
    let lib = format!(
        "{AGENT_SKILL_HEADER}\npub mod util;\nuse util::helper;\n\
         pub fn uses() -> usize {{ helper() }}\n\
         pub fn local() -> String {{ render_agent_protocol_skill(&[], SkillHarness::Omp) }}\n"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"skill\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(root.join("src/lib.rs"), &lib).expect("lib");
    fs::write(root.join("src/util.rs"), "pub fn helper() -> usize { 1 }\n").expect("util");
    let path = root.join("src/lib.rs");
    let path = path.to_str().expect("utf-8");

    // The candidate a host takes from the scope facts: the first use of an
    // imported name. The `_` of `as _` and of `'_` is never one.
    let opened = bage::inspect::open_bytes(&bage::parser::Adapter::new(), path, lib.as_bytes())
        .expect("parse");
    let bage::inspect::Scopes::Supported(scopes) = bage::inspect::extract_scopes(&opened) else {
        panic!("rust has a scope model");
    };
    let first = scopes
        .occurrences
        .iter()
        .find(|o| o.resolution == bage::inspect::Resolution::ImportedName)
        .expect("a use of an imported name");
    assert_eq!(first.name, "helper");
    let index = lsp::TextIndex::new(lib.as_bytes());
    let at = |byte: usize| {
        let (line, character) = index.position_at(byte).expect("position");
        lsp::ReadyProbe {
            path,
            content: &lib,
            line,
            character,
        }
    };
    let cross_file = at(first.start_byte);
    let in_file = at(lib.rfind("render_agent_protocol_skill(").expect("call"));
    let blank = at(lib.find("as _").expect("as _") + 3);

    let mut c = Client::new_stdio(&["rust-analyzer".to_string()]).expect("spawn rust-analyzer");
    c.configure(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(30),
        ready_deadline: Duration::from_secs(120),
        ready_retry: Duration::from_millis(250),
        ..ClientConfig::default()
    });
    c.initialize(&lsp::file_uri(root.to_str().expect("utf-8")).to_string())
        .expect("initialize");
    assert!(
        matches!(
            c.await_ready_by(&[cross_file, blank]),
            Err(LspError::BlankProbe { .. })
        ),
        "a candidate on `_` is refused before anything is sent"
    );
    let started = Instant::now();
    assert_eq!(
        c.await_ready_by(&[in_file, cross_file]).expect("ready"),
        1,
        "only the definition in another file shows the server ready"
    );
    eprintln!("ready by the cross-file probe in {:?}", started.elapsed());

    // Loaded or not, an answer inside the probed file never counts. What the
    // server last said is its own business: the in-file location, or, while
    // it reloads after `cargo check`, a "content modified" refusal.
    let mut cfg = c.config();
    cfg.ready_deadline = Duration::from_secs(3);
    c.configure(cfg);
    match c.await_ready_by(&[in_file]) {
        Err(LspError::ReadyProbesDeadline { probes, .. }) => {
            eprintln!("in-file probe, last answer: {}", probes[0].last);
            assert!(
                matches!(
                    probes[0].last,
                    ProbeAnswer::SameFile { .. } | ProbeAnswer::Refused { .. }
                ),
                "{:?}",
                probes[0].last
            );
        }
        other => panic!("an in-file answer was taken as ready: {other:?}"),
    }
    c.close().expect("close");
}
