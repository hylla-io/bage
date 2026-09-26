//! The caller's `initializationOptions` declaration against a REAL server,
//! measured by the one observable that matters: whether the server reaches for
//! a package manager.
//!
//! A TypeScript server runs `npm install --ignore-scripts types-registry@latest`
//! on its own initiative, hitting the network and a user cache nobody declared.
//! The only switch for it rides in `initializationOptions`, so this suite is
//! also the end-to-end proof that the declaration arrives.
//!
//! Accept-vs-control: the SAME session, same fixture, same deadline, differing
//! only in the declared switch. A probe that observes zero calls without the
//! control arm proves nothing — it cannot tell a working switch from a server
//! that was never going to call npm.
//!
//! `npmLocation` (a declaration too) points the server at a counting shim, so
//! nothing is installed and the count is exact. Run with `HOME` pointed at a
//! scratch directory: the server creates its typings cache under it, and the
//! spawn seam takes no declared environment.
//!
//! Runs only with `BAGE_LSP_REAL_TEST=1` and prints a loud SKIP line
//! otherwise; under the opt-in a missing server FAILS rather than skipping,
//! because the operator asked for that tier. `BAGE_TSSERVER_PATH` names a
//! `tsserver.js` for a host whose TypeScript is the native build, which ships
//! none — itself declared through `initializationOptions`.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use bage::lsp::{self, Client, ClientConfig};
use serde_json::{Value, json};

/// How long a call is waited for. The control arm must reach npm inside it,
/// and the declared arm must stay silent for the whole of it.
const ATA_WINDOW: Duration = Duration::from_secs(45);

/// JavaScript, and the fixture carries no `tsconfig`: type acquisition is a
/// property of an INFERRED JS project, and a configured TypeScript one never
/// reaches for `@types` at all.
const PROBE_FILE: &str = "index.js";
const PROBE_SOURCE: &str = "const _ = require(\"lodash\");\nmodule.exports = _.size([1]);\n";

fn real_enabled() -> bool {
    std::env::var("BAGE_LSP_REAL_TEST").is_ok_and(|v| v == "1")
}

/// A fixture whose dependency has no bundled types, which is what sends the
/// server looking for `@types` in the first place.
fn write_fixture(root: &Path) {
    fs::write(
        root.join("package.json"),
        "{\"name\":\"ata-probe\",\"version\":\"1.0.0\",\"dependencies\":{\"lodash\":\"^4.17.21\"}}\n",
    )
    .expect("package.json");
    fs::write(root.join(PROBE_FILE), PROBE_SOURCE).expect("probe source");
}

/// An `npm` that records its invocation and installs nothing. Failing is
/// deliberate: the probe counts the REACH for a package manager, and a
/// succeeding shim would have to fake a registry payload to stay honest.
fn write_npm_shim(dir: &Path, log: &Path) -> String {
    let shim = dir.join("npm");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\nexit 1\n",
            log.to_string_lossy()
        ),
    )
    .expect("shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("shim mode");
    }
    shim.to_string_lossy().into_owned()
}

fn npm_calls(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Opens the fixture under `options` and returns every npm invocation the
/// server made within [`ATA_WINDOW`]. The control arm short-circuits on the
/// first call; the declared arm necessarily waits the window out.
fn npm_calls_during_session(options: Value, stop_at_first: bool) -> Vec<String> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    write_fixture(root);
    let shim_dir = root.join("shim");
    fs::create_dir(&shim_dir).expect("shim dir");
    let log = root.join("npm-calls.log");
    let npm = write_npm_shim(&shim_dir, &log);

    let mut declared = options;
    declared["npmLocation"] = json!(npm);
    if let Ok(tsserver) = std::env::var("BAGE_TSSERVER_PATH") {
        declared["tsserver"] = json!({"path": tsserver});
    }

    let mut c = Client::new_stdio(&[
        "typescript-language-server".to_string(),
        "--stdio".to_string(),
    ])
    .expect("typescript-language-server must be installed for BAGE_LSP_REAL_TEST=1");
    c.configure(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        initialization_options: Some(declared),
        ..ClientConfig::default()
    });
    c.initialize(&lsp::file_uri(&root.to_string_lossy()).to_string())
        .expect("initialize");
    c.did_open(&root.join(PROBE_FILE).to_string_lossy(), PROBE_SOURCE)
        .expect("didOpen");

    let deadline = Instant::now() + ATA_WINDOW;
    while Instant::now() < deadline {
        if stop_at_first && !npm_calls(&log).is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let seen = npm_calls(&log);
    let _ = c.close();
    seen
}

#[test]
fn a_declared_switch_stops_the_server_reaching_for_npm() {
    if !real_enabled() {
        println!("SKIP lsp_typing_acquisition: set BAGE_LSP_REAL_TEST=1 to run");
        return;
    }

    let control = npm_calls_during_session(json!({}), true);
    println!("control arm npm calls: {control:?}");
    assert!(
        !control.is_empty(),
        "control arm saw no npm call, so this probe cannot discriminate: \
         either the server stopped acquiring types or the shim was not reached"
    );

    let declared =
        npm_calls_during_session(json!({"disableAutomaticTypingAcquisition": true}), false);
    println!("declared arm npm calls: {declared:?}");
    assert!(
        declared.is_empty(),
        "the declaration did not reach the server (control saw {control:?})"
    );
}
