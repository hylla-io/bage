//! Formatter and linter ports applied to staged edit content before it is
//! committed, together with configured-command exec adapters and in-memory
//! fakes for tests.
//!
//! Per SPEC §3.5, formatting rewrites staged content and linting blocks the
//! edit on failure: a lint error means the staged content is rejected. Both
//! ports are pure boundaries so the edit pipeline never depends on a
//! concrete tool.

use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};

/// A formatter or linter failure: the tool that failed and the diagnostic
/// message (for command adapters, the exit status plus captured stderr).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{tool}: {message}")]
pub struct ToolError {
    /// The tool (executable or fake) that produced the failure.
    pub tool: String,
    /// The diagnostic text.
    pub message: String,
}

/// Rewrites staged source content. `format` returns the formatted bytes, or
/// an error if the underlying tool fails; on error the staged content is
/// left unchanged by the caller.
pub trait Formatter: Send + Sync {
    /// Returns the formatted form of `src`; callers treat the result as the
    /// new staged content.
    fn format(&self, src: &[u8]) -> Result<Vec<u8>, ToolError>;
}

/// Validates staged source content. `Ok(())` means the content is clean and
/// the edit may proceed; an error is a lint failure that blocks the edit.
pub trait Linter: Send + Sync {
    /// Reports whether `src` passes the configured checks.
    fn lint(&self, src: &[u8]) -> Result<(), ToolError>;
}

/// Runs `name args...` with `src` piped to stdin, returning captured stdout.
/// A launch failure or non-zero exit yields a [`ToolError`] including the
/// captured stderr. Stdin is fed from a separate thread so a tool that
/// streams output while reading input cannot deadlock the pipe pair.
fn run_tool(name: &str, args: &[String], src: &[u8]) -> Result<Vec<u8>, ToolError> {
    let err = |message: String| ToolError {
        tool: name.to_string(),
        message,
    };

    let mut child = Command::new(name)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| err(e.to_string()))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let src = src.to_vec();
    let writer = std::thread::spawn(move || {
        // A tool that exits without draining stdin breaks the pipe; that is
        // its prerogative, and its exit status is the verdict that matters.
        let _ = stdin.write_all(&src);
    });

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(out) = child.stdout.as_mut() {
        out.read_to_end(&mut stdout)
            .map_err(|e| err(e.to_string()))?;
    }
    if let Some(errs) = child.stderr.as_mut() {
        errs.read_to_end(&mut stderr)
            .map_err(|e| err(e.to_string()))?;
    }
    let status = child.wait().map_err(|e| err(e.to_string()))?;
    writer.join().expect("stdin writer panicked");

    if !status.success() {
        return Err(err(format!(
            "{status}: {}",
            String::from_utf8_lossy(&stderr)
        )));
    }
    Ok(stdout)
}

/// A [`Formatter`] backed by an external command. The command is invoked
/// with the source piped to stdin and its stdout taken as the formatted
/// result; a non-zero exit is reported as an error including stderr.
#[derive(Debug, Clone, Default)]
pub struct CmdFormatter {
    /// The executable to run (resolved via PATH).
    pub name: String,
    /// The arguments passed to the executable.
    pub args: Vec<String>,
}

impl Formatter for CmdFormatter {
    fn format(&self, src: &[u8]) -> Result<Vec<u8>, ToolError> {
        run_tool(&self.name, &self.args, src)
    }
}

/// A [`Linter`] backed by an external command. The command is invoked with
/// the source piped to stdin; a zero exit means clean, while a non-zero exit
/// (or launch failure) is a blocking lint failure including stderr.
#[derive(Debug, Clone, Default)]
pub struct CmdLinter {
    /// The executable to run (resolved via PATH).
    pub name: String,
    /// The arguments passed to the executable.
    pub args: Vec<String>,
}

impl Linter for CmdLinter {
    fn lint(&self, src: &[u8]) -> Result<(), ToolError> {
        run_tool(&self.name, &self.args, src).map(|_| ())
    }
}

/// The closure type a [`FakeFormatter`] may delegate to.
pub type FormatFn = dyn Fn(&[u8]) -> Result<Vec<u8>, ToolError> + Send + Sync;

/// An in-memory [`Formatter`] for tests. When `format_fn` is set it is
/// invoked; otherwise `format` is the identity transform.
#[derive(Default)]
pub struct FakeFormatter {
    /// When set, fully determines `format`'s behaviour.
    pub format_fn: Option<Box<FormatFn>>,
}

impl Formatter for FakeFormatter {
    fn format(&self, src: &[u8]) -> Result<Vec<u8>, ToolError> {
        match &self.format_fn {
            Some(f) => f(src),
            None => Ok(src.to_vec()),
        }
    }
}

/// The closure type a [`FakeLinter`] may delegate to.
pub type LintFn = dyn Fn(&[u8]) -> Result<(), ToolError> + Send + Sync;

/// An in-memory [`Linter`] for tests. When `lint_fn` is set it is invoked;
/// otherwise `lint` returns `err` (`None` by default, meaning clean).
#[derive(Default)]
pub struct FakeLinter {
    /// When set, fully determines `lint`'s behaviour.
    pub lint_fn: Option<Box<LintFn>>,
    /// Returned by `lint` when `lint_fn` is `None`. `None` means clean.
    pub err: Option<ToolError>,
}

impl Linter for FakeLinter {
    fn lint(&self, src: &[u8]) -> Result<(), ToolError> {
        match (&self.lint_fn, &self.err) {
            (Some(f), _) => f(src),
            (None, Some(e)) => Err(e.clone()),
            (None, None) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_formatter_identity_by_default() {
        let f = FakeFormatter::default();
        assert_eq!(f.format(b"abc").unwrap(), b"abc");
    }

    #[test]
    fn fake_formatter_delegates_to_fn() {
        let f = FakeFormatter {
            format_fn: Some(Box::new(|src| Ok(src.to_ascii_uppercase()))),
        };
        assert_eq!(f.format(b"abc").unwrap(), b"ABC");
    }

    #[test]
    fn fake_linter_clean_by_default_and_returns_err() {
        assert!(FakeLinter::default().lint(b"x").is_ok());
        let boom = ToolError {
            tool: "fake".into(),
            message: "boom".into(),
        };
        let l = FakeLinter {
            err: Some(boom.clone()),
            ..Default::default()
        };
        assert_eq!(l.lint(b"x").unwrap_err(), boom);
    }

    #[test]
    fn cmd_formatter_pipes_stdin_to_stdout() {
        let f = CmdFormatter {
            name: "tr".into(),
            args: vec!["a-z".into(), "A-Z".into()],
        };
        assert_eq!(f.format(b"hello\n").unwrap(), b"HELLO\n");
    }

    #[test]
    fn cmd_formatter_nonzero_exit_rejects_with_stderr() {
        let f = CmdFormatter {
            name: "sh".into(),
            args: vec!["-c".into(), "echo bad >&2; exit 3".into()],
        };
        let err = f.format(b"x").unwrap_err();
        assert_eq!(err.tool, "sh");
        assert!(err.message.contains("bad"), "{err}");
    }

    #[test]
    fn cmd_formatter_missing_binary_rejects() {
        let f = CmdFormatter {
            name: "definitely-not-a-real-binary-bage".into(),
            args: vec![],
        };
        assert!(f.format(b"x").is_err());
    }

    #[test]
    fn cmd_linter_zero_exit_is_clean_nonzero_blocks() {
        let clean = CmdLinter {
            name: "sh".into(),
            args: vec!["-c".into(), "cat >/dev/null".into()],
        };
        assert!(clean.lint(b"fine\n").is_ok());

        let dirty = CmdLinter {
            name: "sh".into(),
            args: vec!["-c".into(), "echo nope >&2; exit 1".into()],
        };
        let err = dirty.lint(b"fine\n").unwrap_err();
        assert!(err.message.contains("nope"), "{err}");
    }
}
