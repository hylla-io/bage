//! Declared-session bounds and public document sync, driven through the
//! crate's PUBLIC API only — a consumer outside the crate must be able to
//! declare every time bound before `initialize` and drive `didOpen`/`didClose`
//! itself.
//!
//! The bound tests use a real subprocess that speaks no LSP at all (`sleep`):
//! it never answers `initialize` or `shutdown` and never exits on `exit`, which
//! is exactly the server a declared bound must cut short.
//!
//! The real-server cases run only with `BAGE_LSP_REAL_TEST=1` and print a loud
//! SKIP line otherwise; a missing server under the opt-in FAILS rather than
//! skipping, because the operator asked for that tier.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bage::lsp::{self, Client, ClientConfig, LspError, LspPool, ReadyFailure};

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
    assert_eq!(d.stderr_tail_bytes, 16 * 1024);
}

/// rust-analyzer's status extension, as a caller declares it.
fn rust_analyzer_failed() -> ReadyFailure {
    ReadyFailure {
        method: "experimental/serverStatus".to_string(),
        when: serde_json::json!({"health": "error", "quiescent": true}),
        message_pointer: Some("/message".to_string()),
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
    c.configure(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        ready_deadline: Duration::from_secs(120),
        ready_retry: Duration::from_millis(250),
        ..ClientConfig::default()
    });
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
fn did_open_buffer_is_what_a_real_server_resolves_against() {
    if std::env::var("BAGE_LSP_REAL_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP did_open_buffer_is_what_a_real_server_resolves_against: set BAGE_LSP_REAL_TEST=1 to run"
        );
        return;
    }
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

/// Starts rust-analyzer on [`ORPHAN_MEMBER`] under `cfg` and gates on a call
/// that a loaded workspace would resolve.
fn await_orphan_member(cfg: ClientConfig) -> (Result<(), LspError>, Duration) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonical root");
    for (rel, content) in ORPHAN_MEMBER {
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
        ORPHAN_MEMBER[1].1,
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
fn rust_analyzer_reported_failure_stops_readiness_at_once() {
    if std::env::var("BAGE_LSP_REAL_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP rust_analyzer_reported_failure_stops_readiness_at_once: set BAGE_LSP_REAL_TEST=1 to run"
        );
        return;
    }
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

    // Undeclared: probing alone decides, exactly as before, and the deadline
    // error carries what the server said on stderr.
    let (got, took) = await_orphan_member(ClientConfig {
        ready_deadline: Duration::from_secs(8),
        experimental_capabilities: None,
        ready_failures: Vec::new(),
        ..declared.clone()
    });
    match got {
        Err(LspError::ReadyDeadline { ref stderr, .. }) => {
            assert!(stderr.captured);
            assert!(
                stderr.text.contains("workspace"),
                "the server's own explanation must ride on the error: {stderr:?}"
            );
            assert_prints_stderr(got.as_ref().unwrap_err(), stderr);
        }
        other => panic!("want ReadyDeadline, got {other:?}"),
    }
    assert!(
        took >= Duration::from_secs(8),
        "undeclared must wait out the deadline: {took:?}"
    );
}
