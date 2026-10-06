//! The environment a server started in its workspace runs with.
//!
//! One test, alone in its own binary: it sets a variable on this process so
//! the inheritance is under test rather than assumed, and changing the
//! environment is only sound while no other thread reads it.

use std::path::Path;
use std::time::{Duration, Instant};

use bage::lsp::{Client, ClientConfig};

/// A stdio child that prints `RUSTUP_TOOLCHAIN` and `HOME` (or `unset`) to
/// stderr, then waits.
fn env_server() -> Vec<String> {
    [
        "sh",
        "-c",
        "echo \"${RUSTUP_TOOLCHAIN-unset} ${HOME-unset}\" >&2; sleep 30",
    ]
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

fn line_of(c: Client) -> String {
    let line = first_stderr_line(&c);
    quick_close(c);
    line
}

#[test]
fn a_server_started_in_its_workspace_never_inherits_a_rustup_toolchain() {
    // SAFETY: the only test in this binary, so no other thread of it reads
    // the environment while this writes it.
    unsafe {
        std::env::set_var("RUSTUP_TOOLCHAIN", "inherited-toolchain");
        std::env::set_var("HOME", "/inherited/home");
    }
    let dir = tempfile::tempdir().expect("tempdir");

    // The control: a plain spawn inherits the variable, so its absence below
    // is the workspace spawn's doing.
    let c = Client::new_stdio(&env_server()).expect("spawn sh");
    assert_eq!(line_of(c), "inherited-toolchain /inherited/home");

    let c = Client::new_stdio_in(&env_server(), dir.path()).expect("spawn sh");
    assert_eq!(
        line_of(c),
        "unset /inherited/home",
        "the workspace's toolchain file must decide, not an inherited RUSTUP_TOOLCHAIN"
    );

    // What the caller declares is applied on top, a toolchain included.
    let home = dir.path().join("home");
    let c = Client::new_stdio_in_with_env(&env_server(), dir.path(), [("HOME", home.as_os_str())])
        .expect("spawn sh");
    assert_eq!(line_of(c), format!("unset {}", home.display()));

    let c = Client::new_stdio_in_with_env(
        &env_server(),
        dir.path(),
        [("RUSTUP_TOOLCHAIN", Path::new("declared").as_os_str())],
    )
    .expect("spawn sh");
    assert_eq!(line_of(c), "declared /inherited/home");
}
