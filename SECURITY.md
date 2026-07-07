# Security Policy

## Supported versions

Båge is pre-1.0; only the latest `v0.x` release on `main` receives security fixes. Older tags
are not patched — pin to the newest release.

## Reporting a vulnerability

Please report suspected vulnerabilities **privately**, never via public issues or PRs:

- **Preferred:** open a private [security advisory](https://github.com/hylla-io/bage/security/advisories/new).
- **Or email:** evan@hylla.io

Include a description, the affected version/commit, and a reproduction if you have one. You will
get an acknowledgement within 7 days and a fix or mitigation timeline after triage. Please allow
a reasonable disclosure window before any public discussion.

## Scope

Båge edits files on the machine it runs on and can drive a configured language server, formatter,
and linter. Treat input files, the `--lsp`/formatter/linter commands, and the WAL and clipboard
directories (`$BAGE_CLIPBOARD`) as trusted local paths. In scope: code paths that let untrusted
input escape those boundaries — path traversal on write, command injection via a configured tool,
or WAL/clipboard tampering that leads to silent corruption of a file Båge writes.
