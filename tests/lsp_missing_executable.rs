//! A server's missing tool is actionable before a readiness deadline.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Trace(Mutex<Vec<lsp::RequestTraceEvent>>);
impl lsp::RequestTraceSink for Trace {
    fn record(&self, event: &lsp::RequestTraceEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

use bage::lsp::{self, Client, ClientConfig, MissingExecutablePattern, ReadyFailure, ReadyMode};

#[test]
#[ignore = "real gopls: BAGE_LSP_REAL_TEST=1 cargo test --test lsp_missing_executable -- --ignored"]
fn gopls_missing_go_stops_readiness_before_the_deadline() {
    assert_eq!(std::env::var("BAGE_LSP_REAL_TEST").as_deref(), Ok("1"));
    let gopls = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("gopls"))
        .find(|file| file.is_file())
        .expect("real gopls on PATH")
        .canonicalize()
        .expect("gopls path");
    let go_root = std::process::Command::new("go")
        .args(["env", "GOROOT"])
        .output()
        .expect("resolve installed Go toolchain");
    assert!(go_root.status.success(), "{:?}", go_root.stderr);
    let go_root = std::path::PathBuf::from(String::from_utf8(go_root.stdout).unwrap().trim());
    for available in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("root");
        let tools = root.join("child-path");
        std::fs::create_dir(&tools).expect("child PATH");
        if available {
            #[cfg(unix)]
            std::os::unix::fs::symlink(go_root.join("bin/go"), tools.join("go")).unwrap();
            #[cfg(not(unix))]
            std::fs::copy(go_root.join("bin/go.exe"), tools.join("go.exe")).unwrap();
        }
        let content = "package t\n\nfunc f() int { return helper() }\n";
        std::fs::write(root.join("go.mod"), "module example.com/t\n\ngo 1.23\n").unwrap();
        std::fs::write(root.join("a.go"), content).unwrap();
        std::fs::write(
            root.join("b.go"),
            "package t\nfunc helper() int { return 1 }\n",
        )
        .unwrap();
        let cache = root.join("go-cache");
        let modules = root.join("go-mod-cache");
        let mut client = Client::new_stdio_in_with_env(
            &[gopls.to_str().expect("utf8 gopls").to_owned()],
            &root,
            [
                ("PATH", tools.as_os_str()),
                ("HOME", root.as_os_str()),
                ("GOCACHE", cache.as_os_str()),
                ("GOMODCACHE", modules.as_os_str()),
                ("GOROOT", go_root.as_os_str()),
                ("GOTOOLCHAIN", std::ffi::OsStr::new("local")),
            ],
        )
        .expect("spawn isolated gopls");
        client
            .configure(ClientConfig {
                initialize_timeout: Duration::from_secs(10),
                ready_deadline: if available {
                    Duration::from_secs(60)
                } else {
                    Duration::from_secs(8)
                },
                call_timeout: Duration::from_secs(1),
                ready_retry: Duration::from_millis(20),
                missing_executable_patterns: vec![MissingExecutablePattern {
                    prefix: "exec: \"".into(),
                    suffix: "\": executable file not found in $PATH".into(),
                }],
                ready_failures: ["window/showMessage", "window/logMessage"]
                    .into_iter()
                    .map(|method| ReadyFailure {
                        method: method.to_owned(),
                        when: serde_json::json!({"type": 1}),
                        message_pointer: Some("/message".to_owned()),
                        mode: ReadyMode::Once,
                    })
                    .collect(),
                shutdown_timeout: Duration::from_secs(1),
                exit_deadline: Duration::from_secs(1),
                ..ClientConfig::default()
            })
            .expect("valid configuration");
        let trace = Arc::new(Trace::default());
        client.set_request_trace(format!("gopls-{available}"), Some(trace.clone()));
        client
            .initialize(&lsp::file_uri(root.to_str().unwrap()).to_string())
            .expect("initialize");
        let started = Instant::now();
        let outcome = client.await_ready(root.join("a.go").to_str().unwrap(), content, 2, 22);
        let elapsed = started.elapsed();
        let closed = client.close();
        if available {
            outcome.expect("the same module loads with Go on the isolated PATH");
        } else {
            let error = outcome.expect_err("go is absent only in the server's child environment");
            assert!(
                matches!(&error, lsp::LspError::MissingExecutable { executable, .. } if executable == "go"),
                "{error:?}"
            );
            let message = error.to_string();
            assert!(
                elapsed < Duration::from_secs(4),
                "terminal error waited {elapsed:?}: {error:?}"
            );
            assert!(
                message.contains("put `go` on PATH") && !message.contains("longer"),
                "{message}"
            );
        }
        // A server still loading may not answer shutdown; close must still reap it.
        if let Err(error) = closed {
            assert!(
                matches!(error, lsp::LspError::Timeout { ref method, .. } if method == "shutdown"),
                "{error:?}"
            );
        }
        assert_eq!(
            client.server_pid(),
            None,
            "close relinquishes the reaped child"
        );
        let events = trace.0.lock().unwrap();
        let initialized: Vec<_> = events
            .iter()
            .filter(|event| event.method == "initialize")
            .collect();
        assert_eq!(initialized.len(), 2, "{events:?}");
        assert_eq!(initialized[0].hop, lsp::RequestTraceHop::Send);
        assert_eq!(initialized[1].hop, lsp::RequestTraceHop::Reply);
        assert_eq!(initialized[0].request_id, initialized[1].request_id);
        assert!(
            events
                .iter()
                .all(|event| event.session == format!("gopls-{available}"))
        );
    }
}
