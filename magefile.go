//go:build mage

// Mage build automation — canonical Go-only sibling magefile.
//
// This file is BYTE-IDENTICAL across fresh Go-only siblings (lagom, bage, and
// any future fresh Go sibling). It is project-agnostic: `Build` globs every
// cmd/<name>/main.go and builds each to bin/<name>, so the same file works in
// any module without per-project edits. Test flags ride the generic
// MAGE_GO_TEST_FLAGS env var.
//
// Run "mage -l" to list targets. The top-level gate is "mage ci" which runs
// FormatCheck, Vet, Cover (race+cover combined), Tidy.
//
// Canonical 12-target shape (per tillsyn P6 — naming MUST stay identical across
// all sibling projects so dispatched agents always know the gate name):
//
//	TestFunc(pkg, fn)  builder + build-QA       go test -run "^<Func>$" -count=1 -race <pkg>
//	TestPkg(pkg)       plan-QA read-only        go test -count=1 <pkg>
//	Test               closeout/orch            go test ./...
//	RacePkg(pkg)       build-QA                 go test -race -count=1 <pkg>
//	Race               closeout/orch            go test -race ./...
//	FormatFile(file)   builder + build-QA       gofumpt -w <file>
//	Format             closeout/orch            gofumpt -w .
//	FormatCheck        ci                       gofumpt -l . && fail if non-empty
//	VetPkg(pkg)        builder + build-QA       go vet <pkg>
//	Vet                closeout/orch            go vet ./...
//	Tidy               orch-only                go mod tidy + diff-exit-code
//	CI                 closeout/orch            FormatCheck + Vet + Cover + Tidy
//
// Hyphenated aliases (format-check / format-file / test-func / test-pkg /
// race-pkg / vet-pkg / check) preserved for human ergonomics.
//
// Compile prerequisite (see R_SHIP_HANDOFF.md): `go mod init <module>` then
// `go get github.com/evanmschultz/laslig@latest && go mod tidy` to resolve the
// laslig/gotestout imports below to their LATEST versions. Until that runs this
// magefile will not compile — the names + shape are in place for cross-sibling
// consistency.
//
// Test output is rendered through laslig/gotestout, which auto-detects TTY:
// humans get a styled summary, agents/CI pipes get plain text.
package main

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"github.com/evanmschultz/laslig"
	"github.com/evanmschultz/laslig/gotestout"
)

const binDir = "bin"

// localBuildVCSFlag disables VCS stamping so `go build` stays quiet in
// bare-worktree checkouts that confuse Go's VCS auto-detection.
const localBuildVCSFlag = "-buildvcs=false"

// Aliases preserves the familiar hyphenated task names while keeping the visible target list small.
var Aliases = map[string]interface{}{
	"check":        CI,
	"fmt":          Format,
	"fmt-check":    FormatCheck,
	"format-check": FormatCheck,
	"format-file":  FormatFile,
	"test-func":    TestFunc,
	"test-pkg":     TestPkg,
	"race-pkg":     RacePkg,
	"vet-pkg":      VetPkg,
}

// Build compiles every cmd/<name>/main.go to bin/<name>. Project-agnostic: it
// discovers entrypoints by globbing, so the same magefile builds any module's
// binaries without edits. Warns + skips cleanly when no cmd/*/main.go exists
// yet (fresh-bootstrap project with no entrypoint).
func Build() error {
	mains, err := filepath.Glob("cmd/*/main.go")
	if err != nil {
		return fmt.Errorf("glob cmd entrypoints: %w", err)
	}
	if len(mains) == 0 {
		fmt.Fprintln(os.Stderr, "WARN: no cmd/*/main.go found; skipping build (fresh-bootstrap project)")
		return nil
	}
	if err := os.MkdirAll(binDir, 0o755); err != nil {
		return err
	}
	for _, main := range mains {
		name := filepath.Base(filepath.Dir(main))
		if err := run("go", "build", localBuildVCSFlag, "-o", binDir+"/"+name, "./cmd/"+name); err != nil {
			return fmt.Errorf("build cmd/%s: %w", name, err)
		}
	}
	return nil
}

// Test runs the full test suite without race detection or coverage.
// Closeout/orchestrator surface — fastest all-package gate.
func Test() error {
	return runGoTest("./...")
}

// TestPkg runs every test in ONE package path without race detection.
// Plan-QA read-only surface.
func TestPkg(pkg string) error {
	if pkg == "" {
		return fmt.Errorf("testPkg: package path required (e.g. mage testPkg ./internal/ops)")
	}
	return runGoTest(pkg, "-count=1")
}

// TestFunc runs ONE named test function in ONE package path with race
// detection and no result caching. Builder + build-QA surface.
func TestFunc(pkg, testName string) error {
	pkg = strings.TrimSpace(pkg)
	testName = strings.TrimSpace(testName)
	if pkg == "" {
		return fmt.Errorf("testFunc: package path required (e.g. mage testFunc ./internal/ops TestMyThing)")
	}
	if testName == "" {
		return fmt.Errorf("testFunc: test function name required (e.g. mage testFunc ./internal/ops TestMyThing)")
	}
	return runGoTest(pkg, "-run", "^"+testName+"$", "-race", "-count=1")
}

// Race runs the full test suite over every package with the race detector enabled.
// Closeout/orchestrator surface.
func Race() error {
	return runGoTest("./...", "-race")
}

// RacePkg runs tests with the race detector for ONE package path.
// Build-QA surface.
func RacePkg(pkg string) error {
	if pkg == "" {
		return fmt.Errorf("racePkg: package path required (e.g. mage racePkg ./internal/ops)")
	}
	return runGoTest(pkg, "-race", "-count=1")
}

// Cover produces a function-level coverage report (race + cover combined for
// the CI gate). No coverage floor is enforced here — fresh siblings start at
// 0% and grow; enforce a threshold in CI once the project has real packages.
func Cover() error {
	if err := runGoTest("./...", "-race", "-coverprofile=coverage.out"); err != nil {
		return err
	}
	return run("go", "tool", "cover", "-func=coverage.out")
}

// runGoTest invokes `go test -json [extraArgs] [$MAGE_GO_TEST_FLAGS] <pkg>` and
// pipes the event stream through laslig/gotestout for TTY-aware rendering. NO
// race detection or coverage in the baseline — callers add `-race`/`-cover`/etc.
//
// The MAGE_GO_TEST_FLAGS envvar is whitespace-tokenized and each non-empty
// token appended so callers can flip `-update`, `-race=false`, `-timeout=30s`,
// or any combination without forcing a magefile edit. Empty / unset is a no-op.
func runGoTest(pkg string, extraArgs ...string) error {
	args := append([]string{"test", "-json"}, extraArgs...)
	for _, tok := range strings.Fields(os.Getenv("MAGE_GO_TEST_FLAGS")) {
		args = append(args, tok)
	}
	args = append(args, pkg)
	cmd := exec.Command("go", args...)
	cmd.Stderr = os.Stderr
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	if err := cmd.Start(); err != nil {
		return err
	}
	summary, renderErr := gotestout.Render(os.Stderr, stdout, gotestout.Options{
		Policy: laslig.Policy{Format: laslig.FormatAuto},
		View:   gotestout.ViewCompact,
	})
	_, _ = io.Copy(io.Discard, stdout)
	waitErr := cmd.Wait()
	if waitErr != nil {
		return fmt.Errorf("go test failed (tests-failed=%d, build-errors=%d): %w",
			summary.TestsFailed, summary.BuildErrors, waitErr)
	}
	if renderErr != nil {
		return fmt.Errorf("gotestout render: %w", renderErr)
	}
	return nil
}

// Vet runs go vet across the module.
func Vet() error {
	return run("go", "vet", "./...")
}

// VetPkg runs go vet over ONE package path. Builder + build-QA surface.
func VetPkg(pkg string) error {
	if pkg == "" {
		return fmt.Errorf("vetPkg: package path required (e.g. mage vetPkg ./internal/ops)")
	}
	return run("go", "vet", pkg)
}

// Format formats sources in place via gofumpt (latest). Auto-installs gofumpt
// to GOBIN if missing.
func Format() error {
	if err := ensureGofumpt(); err != nil {
		return err
	}
	return run("gofumpt", "-w", ".")
}

// FormatFile rewrites ONE file (or directory) with gofumpt. Builder + build-QA surface.
func FormatFile(path string) error {
	path = strings.TrimSpace(path)
	if path == "" {
		return fmt.Errorf("formatFile: path required (e.g. mage formatFile internal/ops/foo.go)")
	}
	if _, err := os.Stat(path); err != nil {
		return fmt.Errorf("formatFile: %w", err)
	}
	if err := ensureGofumpt(); err != nil {
		return err
	}
	return run("gofumpt", "-w", path)
}

// FormatCheck fails if any file is not gofumpt-clean.
func FormatCheck() error {
	if err := ensureGofumpt(); err != nil {
		return err
	}
	out, err := exec.Command("gofumpt", "-l", ".").Output()
	if err != nil {
		return err
	}
	if len(strings.TrimSpace(string(out))) > 0 {
		fmt.Fprint(os.Stderr, string(out))
		return fmt.Errorf("files are not gofumpt-clean (run `mage format`)")
	}
	return nil
}

// ensureGofumpt makes `gofumpt` resolvable on PATH by installing latest from upstream.
func ensureGofumpt() error {
	if _, err := exec.LookPath("gofumpt"); err == nil {
		return nil
	}
	return run("go", "install", "mvdan.cc/gofumpt@latest")
}

// Tidy runs go mod tidy and fails if go.mod or go.sum changed.
func Tidy() error {
	before, err := snapshot("go.mod", "go.sum")
	if err != nil {
		return err
	}
	if err := run("go", "mod", "tidy"); err != nil {
		return err
	}
	after, err := snapshot("go.mod", "go.sum")
	if err != nil {
		return err
	}
	if before != after {
		return fmt.Errorf("go.mod or go.sum changed; commit the tidy result")
	}
	return nil
}

// CI is the composite Go gate: FormatCheck, Vet, Cover (race+cover combined), Tidy.
func CI() error {
	for _, step := range []func() error{FormatCheck, Vet, Cover, Tidy} {
		if err := step(); err != nil {
			return err
		}
	}
	return nil
}

// Clean removes build artifacts.
func Clean() error {
	return os.RemoveAll(binDir)
}

func run(name string, args ...string) error {
	cmd := exec.Command(name, args...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func snapshot(paths ...string) (string, error) {
	var b strings.Builder
	for _, p := range paths {
		data, err := os.ReadFile(p)
		if err != nil {
			if os.IsNotExist(err) {
				// go.sum may not exist yet on a fresh module — treat as empty.
				continue
			}
			return "", err
		}
		b.WriteString(p)
		b.WriteByte('\n')
		b.Write(data)
		b.WriteByte('\n')
	}
	return b.String(), nil
}
