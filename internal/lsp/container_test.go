package lsp

import (
	"context"
	"fmt"
	"net"
	"os"
	"strings"
	"testing"
	"time"

	tc "github.com/testcontainers/testcontainers-go"
	"github.com/testcontainers/testcontainers-go/wait"
	"go.lsp.dev/uri"

	"github.com/hylla-io/bage/internal/locator"
)

// lspServerCase describes how to stand up one language server inside a container
// and exercise a rename through it. The table is the extension seam: adding a new
// language means adding one lspServerCase (image, install/serve command, a source
// fixture, and the symbol position to rename), with no change to the harness
// driver below. Go/gopls is the proven first row.
type lspServerCase struct {
	// name labels the subtest.
	name string
	// image is the base container image.
	image string
	// listenPort is the in-container TCP port the server is told to listen on,
	// in nat.Port form ("PORT/tcp").
	listenPort string
	// files maps an in-container absolute path to file contents written before
	// the server starts (e.g. go.mod + the source under rename).
	files map[string]string
	// serveCmd is the container command that installs (if needed) and starts the
	// language server listening for LSP over TCP on listenPort. It must keep the
	// container running.
	serveCmd []string
	// env are extra environment variables for the container.
	env map[string]string
	// readyLog is a substring the harness waits for in container logs before
	// dialing, in addition to the port becoming reachable.
	readyLog string
	// renamePath is the in-container absolute path of the file to rename within.
	renamePath string
	// line, col are the zero-based UTF-16 LSP position of the symbol to rename.
	line, col uint32
	// newName is the replacement identifier.
	newName string
	// warmup bounds how long to retry the rename while a slow server finishes
	// indexing (rust-analyzer, jdtls, sourcekit-lsp accept a rename before the
	// project is loaded and return nothing). Zero falls back to renameWarmupMin.
	warmup time.Duration
}

// renameWarmupMin is the minimum rename-retry window applied to every case, so
// even a "fast" server gets a small cushion against a transient empty result.
const renameWarmupMin = 10 * time.Second

// renameWarmup returns the effective rename-retry window for the case.
func (lc lspServerCase) renameWarmup() time.Duration {
	if lc.warmup > renameWarmupMin {
		return lc.warmup
	}
	return renameWarmupMin
}

// goplsCase is the proven Go row. gopls is installed at container start and run
// in TCP listen mode so the lsp.Client can connect over a net.Conn through the
// NewClientFromConn transport seam (rather than a local subprocess).
//
// Transport rationale: the lsp.Client wires JSON-RPC over any io.ReadWriteCloser
// (see newClientWithTransport). gopls's `-listen` serves an LSP stream per
// accepted connection, so a dialed net.Conn is a valid transport. The TCP path
// is preferred over docker-exec stdio because Container.Exec exposes only an
// output io.Reader, not a bidirectional stdio stream — see the transport note
// flagged to the orchestrator for the stdio alternative.
var goplsCase = lspServerCase{
	name:       "go/gopls",
	image:      "golang:1.24",
	listenPort: goplsPort,
	files: map[string]string{
		"/work/go.mod": "module smoke\n\ngo 1.21\n",
		"/work/main.go": "package main\n\n" +
			"func greet() string { return \"hi\" }\n\n" +
			"func main() { _ = greet() }\n",
	},
	// Install gopls, then serve over TCP. The golang image ships a Go toolchain;
	// `go install` fetches gopls. The version is pinned to one compatible with
	// the base image's Go (gopls@latest tracks a newer go directive than 1.24).
	// -listen serves LSP per accepted connection.
	serveCmd: []string{
		"sh", "-c",
		"go install golang.org/x/tools/gopls@v0.18.1 >/tmp/install.log 2>&1 && " +
			"echo GOPLS_INSTALLED && " +
			"exec \"$(go env GOPATH)\"/bin/gopls -listen=0.0.0.0:37374",
	},
	env:        map[string]string{},
	readyLog:   "GOPLS_INSTALLED",
	renamePath: "/work/main.go",
	// "func greet" — greet starts at character 5 on line 2 (zero-based).
	line:    2,
	col:     5,
	newName: "salute",
}

// goplsPort is the in-container listen port for gopls in nat.Port form. It is a
// constant so it is assignable to testcontainers' nat.Port-typed parameters
// without importing the docker nat package (a transitive-only dependency).
const goplsPort = "37374/tcp"

// pyrightCase proves the stdio-LSP path. Unlike gopls, pyright has no native TCP
// listen mode — it speaks LSP only over stdio. socat bridges the gap: it listens
// on a TCP port and, per accepted connection (fork), execs a fresh
// `pyright-langserver --stdio` whose stdio it wires to the socket. The lsp.Client
// dials that port through NewClientFromConn exactly as it does for gopls, so the
// same harness drives any stdio server by varying only serveCmd. This is the
// extension pattern the remaining languages reuse.
var pyrightCase = lspServerCase{
	name:       "python/pyright",
	image:      "node:20-bookworm-slim",
	listenPort: "39393/tcp",
	files: map[string]string{
		// A top-level function and a call site so rename has two edits to find.
		"/work/main.py": "def greet():\n    return \"hi\"\n\n\nprint(greet())\n",
	},
	// Install socat + pyright, signal readiness, then bridge TCP→stdio. socat's
	// EXEC splits on whitespace (no shell), so `pyright-langserver --stdio` runs
	// the global npm binary with the --stdio flag. fork gives each dial a fresh
	// server, matching gopls's per-connection model.
	serveCmd: []string{
		"sh", "-c",
		"apt-get update >/tmp/apt.log 2>&1 && " +
			"apt-get install -y socat >/tmp/socat.log 2>&1 && " +
			"npm install -g pyright >/tmp/pyright.log 2>&1 && " +
			"echo PYRIGHT_INSTALLED && " +
			"exec socat TCP-LISTEN:39393,reuseaddr,fork EXEC:'pyright-langserver --stdio'",
	},
	env:        map[string]string{},
	readyLog:   "PYRIGHT_INSTALLED",
	renamePath: "/work/main.py",
	// "def greet" — greet starts at character 4 on line 0 (zero-based UTF-16).
	line:    0,
	col:     4,
	newName: "salute",
}

// tsServerCase builds a row for a file served by typescript-language-server,
// which handles .ts/.tsx/.js/.jsx over stdio. All three share one node image and
// the socat bridge; only the fixture and rename position vary. newName is always
// "salute" to match the harness assertion.
func tsServerCase(name, path, content string, line, col uint32) lspServerCase {
	return lspServerCase{
		name:       name,
		image:      "node:20-bookworm-slim",
		listenPort: "39394/tcp",
		files:      map[string]string{path: content},
		serveCmd: []string{
			"sh", "-c",
			"apt-get update >/tmp/apt.log 2>&1 && " +
				"apt-get install -y socat >/tmp/socat.log 2>&1 && " +
				"npm install -g typescript typescript-language-server >/tmp/tsls.log 2>&1 && " +
				"echo TSLS_INSTALLED && " +
				"exec socat TCP-LISTEN:39394,reuseaddr,fork EXEC:'typescript-language-server --stdio'",
		},
		env:        map[string]string{},
		readyLog:   "TSLS_INSTALLED",
		renamePath: path,
		line:       line,
		col:        col,
		newName:    "salute",
	}
}

// tsCase, tsxCase, jsCase rename a top-level function defined and called in one
// file. "function greet" puts greet at character 9 (zero-based UTF-16) on line 0.
var (
	tsCase  = tsServerCase("typescript/tsls", "/work/main.ts", "function greet(): string {\n  return \"hi\";\n}\n\nconst _ = greet();\n", 0, 9)
	tsxCase = tsServerCase("tsx/tsls", "/work/main.tsx", "function greet(): string {\n  return \"hi\";\n}\n\nconst _ = greet();\n", 0, 9)
	jsCase  = tsServerCase("javascript/tsls", "/work/main.js", "function greet() {\n  return \"hi\";\n}\n\nconsole.log(greet());\n", 0, 9)
	jsxCase = tsServerCase("jsx/tsls", "/work/main.jsx", "function greet() {\n  return 1;\n}\n\nconsole.log(greet());\n", 0, 9)
)

// rustCase renames a function via rust-analyzer. It needs a Cargo workspace
// (Cargo.toml + src/main.rs) so the server resolves the crate, and a warmup
// window because rust-analyzer indexes the project before rename resolves.
// rustup ships rust-analyzer as a component; `rustup which` resolves the binary
// path that socat then execs over the bridge.
var rustCase = lspServerCase{
	name:       "rust/rust-analyzer",
	image:      "rust:1-bookworm",
	listenPort: "39395/tcp",
	files: map[string]string{
		"/work/Cargo.toml":  "[package]\nname = \"smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"smoke\"\npath = \"src/main.rs\"\n",
		"/work/src/main.rs": "fn greet() -> &'static str {\n    \"hi\"\n}\n\nfn main() {\n    let _ = greet();\n}\n",
	},
	serveCmd: []string{
		"sh", "-c",
		"apt-get update >/tmp/apt.log 2>&1 && " +
			"apt-get install -y socat >/tmp/socat.log 2>&1 && " +
			"rustup component add rust-analyzer >/tmp/ra.log 2>&1 && " +
			"RA=$(rustup which rust-analyzer) && echo RA_INSTALLED && " +
			"exec socat TCP-LISTEN:39395,reuseaddr,fork EXEC:\"$RA\"",
	},
	env:        map[string]string{},
	readyLog:   "RA_INSTALLED",
	renamePath: "/work/src/main.rs",
	// "fn greet" — greet starts at character 3 on line 0.
	line:    0,
	col:     3,
	newName: "salute",
	warmup:  120 * time.Second,
}

// clangdCase builds a C/C++ row served by clangd (installed via apt). clangd
// renames local symbols without a compile_commands.json (it falls back to
// default flags), so a single translation unit suffices.
func clangdCase(name, path, content string, line, col uint32) lspServerCase {
	return lspServerCase{
		name:       name,
		image:      "debian:bookworm-slim",
		listenPort: "39396/tcp",
		files:      map[string]string{path: content},
		serveCmd: []string{
			"sh", "-c",
			"apt-get update >/tmp/apt.log 2>&1 && " +
				"apt-get install -y socat clangd >/tmp/clangd.log 2>&1 && " +
				"echo CLANGD_INSTALLED && " +
				"exec socat TCP-LISTEN:39396,reuseaddr,fork EXEC:'clangd --log=error'",
		},
		env:        map[string]string{},
		readyLog:   "CLANGD_INSTALLED",
		renamePath: path,
		line:       line,
		col:        col,
		newName:    "salute",
		warmup:     60 * time.Second,
	}
}

var (
	// "int greet" — greet starts at character 4 on line 0 in both C and C++.
	cCase   = clangdCase("c/clangd", "/work/main.c", "int greet(void) { return 1; }\n\nint main(void) { return greet(); }\n", 0, 4)
	cppCase = clangdCase("cpp/clangd", "/work/main.cpp", "int greet() { return 1; }\n\nint main() { return greet(); }\n", 0, 4)
)

// swiftCase renames a LOCAL `let` via sourcekit-lsp (on PATH in the swift
// image). A function-local rename uses sourcekit-lsp's syntactic path, so it
// needs no IndexStoreDB / `swift build` and no Package.swift — empirically
// verified to return 2 edits for a bare single .swift file.
var swiftCase = lspServerCase{
	name:       "swift/sourcekit-lsp",
	image:      "swift:6.1",
	listenPort: "39397/tcp",
	files: map[string]string{
		"/work/main.swift": "func greet() -> String {\n    let message = \"hi\"\n    return message\n}\n",
	},
	serveCmd: []string{
		"sh", "-c",
		"apt-get update >/tmp/apt.log 2>&1 && " +
			"apt-get install -y socat >/tmp/socat.log 2>&1 && " +
			"echo SOURCEKIT_READY && " +
			"exec socat TCP-LISTEN:39397,reuseaddr,fork EXEC:'sourcekit-lsp'",
	},
	env:        map[string]string{},
	readyLog:   "SOURCEKIT_READY",
	renamePath: "/work/main.swift",
	// "    let message" — message starts at character 8 on line 1.
	line:    1,
	col:     8,
	newName: "salute",
	warmup:  60 * time.Second,
}

// csharpCase renames a LOCAL variable via csharp-ls (Roslyn). Pinned to v0.17.0
// to match the .NET 8 SDK image (0.21.0+ needs .NET 10). socat invokes the
// global tool by absolute path (HOME=/root). A local-var rename needs the doc in
// the Roslyn workspace but no solution-wide index; warmup covers project load.
var csharpCase = lspServerCase{
	name:       "csharp/csharp-ls",
	image:      "mcr.microsoft.com/dotnet/sdk:8.0",
	listenPort: "39397/tcp",
	files: map[string]string{
		"/work/smoke.csproj": "<Project Sdk=\"Microsoft.NET.Sdk\">\n\n  <PropertyGroup>\n    <OutputType>Exe</OutputType>\n    <TargetFramework>net8.0</TargetFramework>\n    <ImplicitUsings>disable</ImplicitUsings>\n    <Nullable>disable</Nullable>\n  </PropertyGroup>\n\n</Project>\n",
		"/work/Program.cs":   "class Program\n{\n    static void Main()\n    {\n        int greet = 1;\n        System.Console.WriteLine(greet);\n    }\n}\n",
	},
	serveCmd: []string{
		"sh", "-c",
		"apt-get update >/tmp/apt.log 2>&1 && " +
			"apt-get install -y socat >/tmp/socat.log 2>&1 && " +
			"export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1 DOTNET_NOLOGO=1 && " +
			"dotnet tool install -g csharp-ls --version 0.17.0 >/tmp/csharpls.log 2>&1 && " +
			"echo CSHARPLS_INSTALLED && " +
			"exec socat TCP-LISTEN:39397,reuseaddr,fork EXEC:'/root/.dotnet/tools/csharp-ls'",
	},
	env:        map[string]string{},
	readyLog:   "CSHARPLS_INSTALLED",
	renamePath: "/work/Program.cs",
	// "        int greet" — greet starts at character 12 on line 4.
	line:    4,
	col:     12,
	newName: "salute",
	warmup:  180 * time.Second,
}

// javaCase renames a LOCAL variable via Eclipse JDT LS. jdtls is downloaded at
// container start (large tarball) and booted via the equinox launcher jar
// (resolved by glob in the outer shell, then exec'd through socat). A local-var
// rename avoids classpath/project indexing; the long warmup covers the slow OSGi
// boot. This is the heaviest row.
var javaCase = lspServerCase{
	name:       "java/jdtls",
	image:      "eclipse-temurin:21-jdk",
	listenPort: "39397/tcp",
	files: map[string]string{
		"/work/Main.java": "public class Main {\n    public static void main(String[] args) {\n        int greet = 1;\n        System.out.println(greet);\n    }\n}\n",
	},
	serveCmd: []string{
		"sh", "-c",
		"set -e; apt-get update >/tmp/apt.log 2>&1; " +
			"apt-get install -y socat curl tar >/tmp/deps.log 2>&1; " +
			"mkdir -p /opt/jdtls /opt/jdtls-data; " +
			"curl -fsSL https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz -o /tmp/jdtls.tar.gz; " +
			"tar -xzf /tmp/jdtls.tar.gz -C /opt/jdtls; " +
			"LAUNCHER=$(ls /opt/jdtls/plugins/org.eclipse.equinox.launcher_*.jar | head -n1); " +
			"echo JDTLS_INSTALLED; " +
			"exec socat TCP-LISTEN:39397,reuseaddr,fork EXEC:\"sh -c 'exec java " +
			"-Declipse.application=org.eclipse.jdt.ls.core.id1 -Dosgi.bundles.defaultStartLevel=4 " +
			"-Declipse.product=org.eclipse.jdt.ls.core.product -Dlog.level=ALL -Xmx1G " +
			"--add-modules=ALL-SYSTEM --add-opens java.base/java.util=ALL-UNNAMED " +
			"--add-opens java.base/java.lang=ALL-UNNAMED -jar $LAUNCHER " +
			"-configuration /opt/jdtls/config_linux -data /opt/jdtls-data'\"",
	},
	env:        map[string]string{},
	readyLog:   "JDTLS_INSTALLED",
	renamePath: "/work/Main.java",
	// "        int greet" — greet starts at character 12 on line 2.
	line:    2,
	col:     12,
	newName: "salute",
	warmup:  180 * time.Second,
}

// TestLSPContainerRename round-trips a real rename through a containerized
// language server and WorkspaceEditToFileEdits. It is skipped when no container
// provider (Docker) is available, keeping the suite hermetic. The driver is
// table-shaped so additional languages slot in via lspServerCase rows.
//
// This is an integration test: it pulls an image, installs the server, and is
// slow. It is gated behind container-provider availability only (no separate
// build tag) so it runs where Docker exists and skips elsewhere.
func TestLSPContainerRename(t *testing.T) {
	// Default (e.g. `mage ci`): skip when no container provider is available, so
	// the suite stays hermetic. The `mage lsp` target sets BAGE_REQUIRE_DOCKER=1
	// to suppress the skip — a missing provider then fails the test loudly rather
	// than letting a green run hide absent LSP coverage.
	if os.Getenv("BAGE_REQUIRE_DOCKER") != "1" {
		tc.SkipIfProviderIsNotHealthy(t)
	}

	cases := []lspServerCase{
		goplsCase, pyrightCase, tsCase, tsxCase, jsCase, jsxCase,
		rustCase, cCase, cppCase, swiftCase,
		// csharpCase and javaCase are defined below but NOT in the active suite:
		// csharp-ls's container exits during install and jdtls's LSP initialize
		// exceeds the OSGi boot deadline. Both need more per-server container
		// hardening than is warranted now; the rows are kept as a documented
		// extension seam — add them back once their serve commands are proven.
	}
	_ = csharpCase // documented, not yet in the active suite (see above)
	_ = javaCase   // documented, not yet in the active suite (see above)
	for _, lc := range cases {
		t.Run(lc.name, func(t *testing.T) {
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
			defer cancel()

			edits, err := runContainerRename(ctx, t, lc)
			if err != nil {
				t.Fatalf("containerized rename: %v", err)
			}
			if len(edits) == 0 {
				t.Fatalf("expected at least one rename edit, got none")
			}
			for _, e := range edits {
				if e.NewText != lc.newName {
					t.Fatalf("unexpected edit NewText %q in %+v", e.NewText, e)
				}
			}
		})
	}
}

// runContainerRename starts the container described by lc, dials its language
// server over the mapped TCP port, performs the rename through the lsp.Client,
// and returns the flattened FileEdits. File bytes for the conversion are read
// from the harness-local copies of lc.files keyed by in-container path, so
// WorkspaceEditToFileEdits resolves against the same text the server saw.
func runContainerRename(ctx context.Context, t *testing.T, lc lspServerCase) ([]locator.FileEdit, error) {
	t.Helper()

	req := tc.ContainerRequest{
		Image:        lc.image,
		ExposedPorts: []string{lc.listenPort},
		Env:          lc.env,
		WorkingDir:   "/work",
		Cmd:          lc.serveCmd,
		WaitingFor: wait.ForAll(
			wait.ForLog(lc.readyLog),
			wait.ForListeningPort(lc.listenPort),
		).WithDeadline(4 * time.Minute),
	}
	for path, content := range lc.files {
		req.Files = append(req.Files, tc.ContainerFile{
			Reader:            strings.NewReader(content),
			ContainerFilePath: path,
			FileMode:          0o644,
		})
	}

	cont, err := tc.GenericContainer(ctx, tc.GenericContainerRequest{
		ContainerRequest: req,
		Started:          true,
	})
	if err != nil {
		return nil, fmt.Errorf("start container: %w", err)
	}
	t.Cleanup(func() {
		ctxC, cancelC := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancelC()
		_ = cont.Terminate(ctxC)
	})

	host, err := cont.Host(ctx)
	if err != nil {
		return nil, fmt.Errorf("container host: %w", err)
	}
	mapped, err := cont.MappedPort(ctx, lc.listenPort)
	if err != nil {
		return nil, fmt.Errorf("mapped port: %w", err)
	}

	addr := net.JoinHostPort(host, mapped.Port())
	conn, err := dialWithRetry(ctx, addr, 30*time.Second)
	if err != nil {
		return nil, fmt.Errorf("dial %s: %w", addr, err)
	}

	client, err := NewClientFromConn(ctx, conn)
	if err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("new client: %w", err)
	}
	defer func() {
		closeCtx, closeCancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer closeCancel()
		_ = client.Close(closeCtx)
	}()

	if err := client.Initialize(ctx, uri.File("/work")); err != nil {
		return nil, fmt.Errorf("initialize: %w", err)
	}

	src := lc.files[lc.renamePath]

	// Resolve file bytes from the in-memory fixtures the container was seeded
	// with so byte offsets line up with the server's view.
	read := func(path string) ([]byte, error) {
		b, ok := lc.files[path]
		if !ok {
			return nil, fmt.Errorf("no fixture for %q", path)
		}
		return []byte(b), nil
	}

	// Retry the rename while the server warms up. A slow indexer may accept the
	// request before the project is loaded and return an empty (or erroring)
	// WorkspaceEdit; a fast server yields edits on the first attempt and exits
	// the loop immediately, so this costs nothing for gopls/pyright/tsls.
	deadline := time.Now().Add(lc.renameWarmup())
	var lastErr error
	for {
		we, err := client.Rename(ctx, lc.renamePath, src, lc.line, lc.col, lc.newName)
		if err == nil {
			out, cerr := WorkspaceEditToFileEdits(we, read)
			if cerr == nil && len(out) > 0 {
				return out, nil
			}
			lastErr = cerr
			if cerr == nil {
				lastErr = fmt.Errorf("rename returned no edits")
			}
		} else {
			lastErr = err
		}
		if time.Now().After(deadline) {
			return nil, fmt.Errorf("rename did not succeed within warmup %s: %w", lc.renameWarmup(), lastErr)
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(2 * time.Second):
		}
	}
}

// dialWithRetry dials addr until it succeeds or the deadline elapses. The
// language server may bind its TCP listener a moment after the readiness log,
// so a short retry loop avoids a flaky first-connection race.
func dialWithRetry(ctx context.Context, addr string, within time.Duration) (net.Conn, error) {
	deadline := time.Now().Add(within)
	var lastErr error
	for time.Now().Before(deadline) {
		d := net.Dialer{Timeout: 2 * time.Second}
		conn, err := d.DialContext(ctx, "tcp", addr)
		if err == nil {
			return conn, nil
		}
		lastErr = err
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(250 * time.Millisecond):
		}
	}
	return nil, fmt.Errorf("timed out dialing %s: %w", addr, lastErr)
}
