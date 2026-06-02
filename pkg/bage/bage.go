package bage

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"

	"github.com/hylla-io/bage/internal/format"
	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/locator"
	"github.com/hylla-io/bage/internal/lsp"
	"github.com/hylla-io/bage/internal/parser"
	"github.com/hylla-io/bage/internal/parser/treesitter"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/internal/session"

	"go.lsp.dev/uri"
)

// Region is a content-anchored locator into a file: a byte range, the matching
// line/col range, and the region_hash that anchors the region by content
// (SPEC §8.1). See region.Region.
type Region = region.Region

// FileAnchor is the per-file drift gate: a file's RawHash (byte-offset validity)
// and NormHash (whitespace-only drift classifier). See region.FileAnchor.
type FileAnchor = region.FileAnchor

// Edit is a region-anchored edit: replace the bytes of a Region with NewText
// (SPEC §8.1). The model echoes a shown region_hash; it never resends old text.
// See region.Edit.
type Edit = region.Edit

// EditResult is the write-back contract returned to Hylla after a commit: the
// changed byte range plus the recomputed region/file hashes and new line range,
// so Hylla re-ingests only the changed region (SPEC §8.2). See region.EditResult.
type EditResult = region.EditResult

// Plan is the result of a successful Prepare: the durably-logged WAL intent plus
// the region edits and per-file anchors Commit re-validates under lock. See
// session.Plan.
type Plan = session.Plan

// ConflictError reports that a region-anchored edit could not be resolved against
// the live file — the target's region_hash matches no live node (a concurrent
// edit changed the same region) or matches more than one (ambiguous twins). Båge
// rejects rather than guesses (SPEC §8.3, §8.4, ADR-0003). See session.ConflictError.
type ConflictError = session.ConflictError

// ErrConflict is the sentinel wrapped by every ConflictError, matchable with
// errors.Is without inspecting the path. See session.ErrConflict.
var ErrConflict = session.ErrConflict

// Lang enumerates the source languages a parser adapter can parse. See
// parser.Lang.
type Lang = parser.Lang

// Tree is a parsed concrete syntax tree together with its source bytes. See
// parser.Tree.
type Tree = parser.Tree

// Node is a single concrete-syntax-tree node addressed by byte range and point.
// See parser.Node.
type Node = parser.Node

// ParserPort is the engine-agnostic parsing contract Hylla consumes for shared
// ingest parsing. See parser.ParserPort.
type ParserPort = parser.ParserPort

// Hasher computes a stable hex digest of a byte slice. See hashing.Hasher.
type Hasher = hashing.Hasher

// XXHasher is the canonical xxHash64 Hasher shared with Hylla. See
// hashing.XXHasher.
type XXHasher = hashing.XXHasher

// Formatter rewrites staged source content before commit. See format.Formatter.
type Formatter = format.Formatter

// Linter validates staged source content, blocking the edit on failure. See
// format.Linter.
type Linter = format.Linter

// CmdFormatter is an exec-backed Formatter that shells out to a configured
// command. See format.CmdFormatter.
type CmdFormatter = format.CmdFormatter

// CmdLinter is an exec-backed Linter that shells out to a configured command.
// See format.CmdLinter.
type CmdLinter = format.CmdLinter

// Re-exported language constants. Each names a tree-sitter grammar the parser
// adapter can select; see parser for the canonical definitions.
const (
	LangUnknown    = parser.LangUnknown
	LangGo         = parser.LangGo
	LangTypeScript = parser.LangTypeScript
	LangTSX        = parser.LangTSX
	LangJavaScript = parser.LangJavaScript
	LangPython     = parser.LangPython
	LangRust       = parser.LangRust
	LangJava       = parser.LangJava
	LangC          = parser.LangC
	LangCPP        = parser.LangCPP
	LangCSharp     = parser.LangCSharp
	LangRuby       = parser.LangRuby
	LangJSON       = parser.LangJSON
	LangHTML       = parser.LangHTML
	LangCSS        = parser.LangCSS
	LangYAML       = parser.LangYAML
	LangTOML       = parser.LangTOML
	LangXML        = parser.LangXML
	LangMakefile   = parser.LangMakefile
	LangBash       = parser.LangBash
	LangMarkdown   = parser.LangMarkdown
	// LangText is the grammar-free fallback: any file type with no registered
	// grammar opens and round-trips losslessly under it.
	LangText = parser.LangText
)

// NewParser returns a fresh ParserPort backed by the official CGO tree-sitter
// adapter. It lets Hylla use Båge's shared parser for ingest without opening a
// full Editor, so the graph and the files can never disagree on structure.
func NewParser() ParserPort { return treesitter.New() }

// LangForPath selects the language for a file path by extension/basename,
// falling back to the grammar-free text mode (never LangUnknown) so any file can
// be opened and losslessly round-tripped. See parser.LangForPath.
func LangForPath(path string) Lang { return parser.LangForPath(path) }

// Config configures an Editor. WALDir and Lang are required; Hasher defaults to
// XXHasher{} when nil. Formatter and Linter are optional pipeline steps run over
// the staged bytes. LSPCommand names the language-server command (argv) used by
// Rename; it may be empty when rename is not needed.
type Config struct {
	// Lang is the source language the parser uses; required.
	Lang Lang
	// Hasher computes region/file digests; defaults to XXHasher{} when nil.
	Hasher Hasher
	// Formatter, when non-nil, rewrites staged bytes before linting/parsing.
	Formatter Formatter
	// Linter, when non-nil, blocks the edit on a lint failure.
	Linter Linter
	// WALDir is the directory holding the write-ahead log; required.
	WALDir string
	// LSPCommand is the language-server command (argv) used by Rename; optional.
	LSPCommand []string
}

// Editor is the configured FILE-LEG edit engine: the public handle wrapping a
// region-anchored session, a shared parser, and (lazily, per Rename) a
// language-server client. It is the behavior facade consumers drive; data types
// are re-exported as aliases above.
type Editor struct {
	sess       *session.Session
	parser     parser.ParserPort
	hasher     hashing.Hasher
	lang       parser.Lang
	walDir     string
	lspCommand []string
}

// Open validates cfg and wires an Editor: a tree-sitter parser as the
// ParserPort and a session.Session over the configured WALDir, Hasher, Lang,
// Formatter, and Linter. WALDir is required; a nil Hasher defaults to XXHasher{}.
// Lang is OPTIONAL: when LangUnknown (the zero value) each file's language is
// auto-detected from its path via LangForPath, so an agent IDE can open a
// mixed-language tree; when set it forces that language for every file.
func Open(cfg Config) (*Editor, error) {
	if cfg.WALDir == "" {
		return nil, errors.New("bage: Config.WALDir is required")
	}
	hasher := cfg.Hasher
	if hasher == nil {
		hasher = hashing.XXHasher{}
	}
	p := treesitter.New()
	sess := &session.Session{
		Parser:    p,
		Hasher:    hasher,
		Formatter: cfg.Formatter,
		Linter:    cfg.Linter,
		Lang:      cfg.Lang,
		WALDir:    cfg.WALDir,
	}
	return &Editor{
		sess:       sess,
		parser:     p,
		hasher:     hasher,
		lang:       cfg.Lang,
		walDir:     cfg.WALDir,
		lspCommand: cfg.LSPCommand,
	}, nil
}

// Parser returns the Editor's shared ParserPort so Hylla can reuse the exact
// parser Båge edits with, keeping graph ingest and file edits structurally
// consistent.
func (e *Editor) Parser() ParserPort { return e.parser }

// Prepare optimistically stages every region-anchored edit against the live
// files, drift-checks via the per-region region_hash (rejecting a Conflict or
// Ambiguous as a *ConflictError, matchable via errors.Is(err, ErrConflict)),
// runs the optional Formatter/Linter, reparses to prove the result is valid, and
// durably records a WAL intent. It returns a Plan whose staged bytes are not yet
// on disk — Prepare's sole on-disk effect is the WAL record. anchors carries the
// per-file drift gate (SPEC §8.1) for each file the edits touch.
func (e *Editor) Prepare(ctx context.Context, edits []Edit, anchors []FileAnchor) (*Plan, error) {
	return e.sess.Prepare(ctx, edits, anchors)
}

// Commit is the atomic, lossless point: per file, under that file's lock, it
// re-reads the live bytes and re-resolves every edit (resolve-under-lock, so a
// benign concurrent shift lands at the current offset and a same-region conflict
// is rejected), atomic-writes, and returns one EditResult per edit. A
// *ConflictError aborts the commit, leaving the source untouched and the WAL
// intact for Recover; full success clears the WAL.
func (e *Editor) Commit(plan *Plan) ([]EditResult, error) { return e.sess.Commit(plan) }

// Rollback abandons a prepared Plan, discarding the staged edits and clearing
// the WAL; the source files are left untouched.
func (e *Editor) Rollback(plan *Plan) error { return e.sess.Rollback(plan) }

// Recover is the crash path: it replays any WAL intent left in the Editor's
// WALDir, restoring affected files to their pre-Prepare state, then clears the
// WAL. A clean Commit leaves nothing to replay, so Recover is then a no-op.
func (e *Editor) Recover(ctx context.Context) error {
	return e.sess.Recover(ctx, e.walDir)
}

// Apply is the standalone convenience for a one-shot edit: it Prepares the
// edits and, on success, immediately Commits the resulting Plan, returning the
// EditResults. Integrated callers that interleave the FILE leg with a graph leg
// use Prepare/Commit directly so the coordinator controls the commit point.
func (e *Editor) Apply(ctx context.Context, edits []Edit, anchors []FileAnchor) ([]EditResult, error) {
	plan, err := e.sess.Prepare(ctx, edits, anchors)
	if err != nil {
		return nil, err
	}
	results, err := e.sess.Commit(plan)
	if err != nil {
		return nil, fmt.Errorf("bage: apply commit: %w", err)
	}
	return results, nil
}

// Rename performs an LSP-driven rename of the symbol at the zero-based
// (line, col) UTF-16 position in file, then stages the resulting cross-file
// edits as region-anchored edits. It requires Config.LSPCommand: it spawns the
// language server, requests the rename, converts the server's WorkspaceEdit into
// byte-range edits, grounds each as a Region with a content region_hash, builds
// one FileAnchor per file, and Prepares them. It returns the Plan; the caller
// Commits (or Rollbacks) it. The server is shut down before Rename returns.
func (e *Editor) Rename(ctx context.Context, file string, line, col uint32, newName string) (*Plan, error) {
	if len(e.lspCommand) == 0 {
		return nil, errors.New("bage: Rename requires Config.LSPCommand")
	}

	abs, err := filepath.Abs(file)
	if err != nil {
		return nil, fmt.Errorf("bage: rename resolve %q: %w", file, err)
	}
	content, err := os.ReadFile(abs)
	if err != nil {
		return nil, fmt.Errorf("bage: rename read %q: %w", abs, err)
	}

	client, err := lsp.NewClient(ctx, e.lspCommand)
	if err != nil {
		return nil, fmt.Errorf("bage: rename start lsp: %w", err)
	}
	defer func() { _ = client.Close(ctx) }()

	if err := client.Initialize(ctx, uri.File(filepath.Dir(abs))); err != nil {
		return nil, fmt.Errorf("bage: rename initialize: %w", err)
	}

	we, err := client.Rename(ctx, abs, string(content), line, col, newName)
	if err != nil {
		return nil, fmt.Errorf("bage: rename: %w", err)
	}

	fileEdits, err := lsp.WorkspaceEditToFileEdits(we, os.ReadFile)
	if err != nil {
		return nil, fmt.Errorf("bage: rename convert: %w", err)
	}
	if len(fileEdits) == 0 {
		return nil, errors.New("bage: rename: server returned no edits")
	}

	edits, anchors, err := e.groundEdits(fileEdits)
	if err != nil {
		return nil, fmt.Errorf("bage: rename ground: %w", err)
	}
	plan, err := e.sess.Prepare(ctx, edits, anchors)
	if err != nil {
		return nil, fmt.Errorf("bage: rename prepare: %w", err)
	}
	return plan, nil
}

// Close releases the Editor's resources. The parser and session hold no
// long-lived handles between edits (the LSP client is per-Rename), so Close is
// currently a no-op kept for forward-compatible lifecycle management.
func (e *Editor) Close() error { return nil }

// groundEdits converts a flat slice of byte-range FileEdits (from an LSP
// WorkspaceEdit) into region-anchored edits plus one FileAnchor per file. Each
// FileEdit becomes a Region whose byte range carries a region_hash computed from
// that file's live bytes, so Resolve verifies content (Exact in place, benign
// shift re-resolves, real conflict rejects). Files are read once each and the
// per-file anchor is built from those live bytes. Edits are returned in a
// deterministic (path, then start-byte) order.
func (e *Editor) groundEdits(fileEdits []locator.FileEdit) ([]Edit, []FileAnchor, error) {
	byPath := make(map[string][]locator.FileEdit)
	for _, fe := range fileEdits {
		byPath[fe.Path] = append(byPath[fe.Path], fe)
	}

	paths := make([]string, 0, len(byPath))
	for p := range byPath {
		paths = append(paths, p)
	}
	sort.Strings(paths)

	var edits []Edit
	anchors := make([]FileAnchor, 0, len(paths))
	for _, p := range paths {
		live, err := os.ReadFile(p)
		if err != nil {
			return nil, nil, fmt.Errorf("bage: read %q: %w", p, err)
		}
		li := region.NewLineIndex(live)

		group := byPath[p]
		sort.SliceStable(group, func(i, j int) bool { return group[i].StartByte < group[j].StartByte })
		for _, fe := range group {
			if fe.StartByte < 0 || fe.EndByte < fe.StartByte || fe.EndByte > len(live) {
				return nil, nil, fmt.Errorf("bage: edit byte range [%d:%d] out of bounds for %q (len %d)", fe.StartByte, fe.EndByte, p, len(live))
			}
			reg := li.FillLineCols(region.Region{
				Path:       p,
				StartByte:  fe.StartByte,
				EndByte:    fe.EndByte,
				RegionHash: region.HashRegion(live, fe.StartByte, fe.EndByte),
			})
			edits = append(edits, region.Edit{Region: reg, NewText: fe.NewText})
		}
		anchors = append(anchors, region.FileAnchor{
			Path:     p,
			RawHash:  hashing.RawHash(e.hasher, live),
			NormHash: hashing.NormHash(e.hasher, live),
		})
	}
	return edits, anchors, nil
}
