# Båge — Bidirectional Code Knowledge Graph

> **Båge** (Swedish for *bow / arc*) — the project name. Pronounced roughly "BO-geh."
> The metaphor: graph edges are arcs that bridge nodes; edits are arcs that bridge
> graph and file.

A design for a code knowledge graph that is built from tree-sitter + LSP and supports
**round-trip editing**: edits to graph nodes are translated programmatically into
precise edits in the underlying source files, across any common file type in a
codebase (source code, Markdown, TOML, YAML, JSON, LaTeX, Typst, RST, AsciiDoc,
Org-mode, RTF, `.docx`, `.xlsx`, `.pptx`, EPUB, ipynb, etc.).

Target language: **Go**. Standard library where possible, vetted third-party libraries
where not.

---

## 1. Core Mental Model

> **The graph is a projection of the files. Files are the source of truth.**
> Every graph edit compiles down to a precise file edit, which is applied, and then
> the graph is updated by re-parsing (incrementally where possible).

This gives a one-way data flow even though the UX feels bidirectional:

```
user mutates graph node
        │
        ▼
graph edit  ──compile──▶  FileEdit (typed per format)
                              │
                              ▼
                       apply atomically
                              │
                              ▼
                    re-parse changed file(s)
                              │
                              ▼
                update graph nodes + edges
                              │
                              ▼
               notify LSPs via didChange
```

Conflict / drift handling: store a content hash per file in the graph. Before
applying an edit, verify the file hasn't changed underneath. If it has, re-parse
and either rebase the edit or reject it.

---

## 2. Graph Construction Layer

### Syntax — tree-sitter

- Provides a concrete syntax tree with precise `{start_byte, end_byte, start_point, end_point}` on every node.
- Supports incremental reparsing: call `Tree.Edit(...)` to update offsets, then `Parser.Parse(oldTree, newSource)` to reuse unchanged subtrees.
- `Tree.Edit` is **pure in-memory bookkeeping**; it does not read or write files. You handle I/O yourself.
- Use `tree.ChangedRanges(oldTree)` after reparsing to know which graph nodes need refreshing.

**Go bindings:**

- `github.com/tree-sitter/go-tree-sitter` — official bindings.
- `github.com/smacker/go-tree-sitter` — older, more grammars pre-packaged, very mature.

**Document and markup grammars that exist** (in addition to all major programming languages):

| Format        | Grammar                                                     |
|---------------|-------------------------------------------------------------|
| Markdown      | `MDeiml/tree-sitter-markdown` (block + inline dual parser)  |
| LaTeX         | `latex-lsp/tree-sitter-latex` (best-effort; LaTeX is Turing complete) |
| BibTeX        | `latex-lsp/tree-sitter-bibtex`                              |
| Typst         | `uben0/tree-sitter-typst` or `frozolotl/tree-sitter-typst`  |
| RTF           | `GoodNotes/tree-sitter-rtf`                                 |
| XML / DTD     | `tree-sitter-grammars/tree-sitter-xml`                      |
| AsciiDoc      | `cathaysia/tree-sitter-asciidoc`                            |
| reStructuredText | `stsewd/tree-sitter-rst`                                 |
| Org-mode      | `emiasims/tree-sitter-org`                                  |
| YAML/JSON/TOML | standard tree-sitter grammars in their respective repos    |

**What does NOT have a tree-sitter grammar** (and conceptually can't): `.docx`,
`.xlsx`, `.pptx`, `.odt`, `.epub`. These are ZIP containers, not text. After
unzipping, the inner XML can be parsed with `tree-sitter-xml`, but the CST nodes
would be generic (`element`, `start_tag`) rather than document-semantic
(*paragraph*, *run*). For these formats, use the Tier 2 object-model approach
described in §5.

### Semantics — LSP

One language server per language, spoken to over JSON-RPC. Use LSPs for:

- Definitions, references, hover, types
- Cross-file symbol resolution
- `textDocument/rename`, `textDocument/codeAction` — these return `WorkspaceEdit`s you can apply
- Diagnostics for free

**Go libraries:**

- `go.lsp.dev/protocol` — LSP types
- `go.lsp.dev/jsonrpc2` — transport
- Or scavenge from `golang.org/x/tools/gopls/internal/lsp/protocol` if you want gopls' own definitions

**Servers to spawn** (subprocess per language, communicate via stdio):

| Language   | Server                              |
|------------|-------------------------------------|
| Go         | `gopls`                             |
| TypeScript | `typescript-language-server`        |
| Python     | `pyright` or `ruff` / `pylsp`       |
| Rust       | `rust-analyzer`                     |
| C / C++    | `clangd`                            |
| Java       | `jdtls`                             |
| Ruby       | `solargraph` or `ruby-lsp`          |
| YAML       | `yaml-language-server`              |
| JSON       | `vscode-json-languageserver`        |
| TOML       | `taplo`                             |
| Markdown   | `marksman`                          |

The graph layer doesn't care which server — they all speak the same protocol.

### Key insight: use LSP edit primitives

Don't roll your own line/char editor. The LSP spec defines:

- `TextEdit` — a `Range` + replacement string
- `WorkspaceEdit` — multi-file edits

If every graph mutation compiles to a `WorkspaceEdit`, you get rename-across-files
and import updates for free from the language server. Apply them using the LSP
range model (note: it's UTF-16 code units, which matters for non-ASCII).

---

## 3. The `Locator` Abstraction and Node Storage

### What a graph node holds

Each graph node holds **just enough** to: locate the underlying content, display
or edit it semantically, detect drift, and round-trip the user's edit back to
the file. The node does **not** hold the canonical content — that lives in the
file. This is the most important invariant:

> **Files are the source of truth. Graph nodes are projections + locators.**

A node looks like:

```go
type Node struct {
    ID         string
    Locator    Locator           // how to find the content in the file
    DisplayText string           // cached human-readable view (semantic, not raw)
    Hash       string            // content hash for drift detection
    Original   *OriginalContent  // OPTIONAL — see "Recreation Information" below
    Metadata   map[string]any    // language, kind, parent symbol, etc.
}
```

### Recreation information — when you need it

By default, the graph **does not** need to store the bytes required to reconstruct
the file. The file IS those bytes. You only need recreation info when one of these
is true:

1. **Offline / disconnected editing.** User edits the graph without the file mounted.
2. **Per-node undo across sessions.** You want fine-grained rollback history.
3. **Lossy view + lossless edit-back.** You show a clean text view (e.g., from
   pandoc) but need to splice the edit back into the original rich format.

When any of these apply, store an `OriginalContent` blob on the node:

```go
type OriginalContent struct {
    Bytes  []byte // exact bytes of the node's region in the file
    Format string // "docx-run-xml", "pandoc-block-json", "raw-text"
}
```

For text formats this is just a slice. For Office formats it's the original `<w:r>`
or `<w:p>` XML fragment. For pandoc-mediated formats it's the original block of
the source format AND the pandoc subtree it produced — see §5.5 for the diff-back
trick.

### Different file formats need different ways to point at a region

```go
type Locator interface {
    Path() string
    Apply(edit Edit) error
    Hash() string // content hash of the target region for drift detection
}

type Edit struct {
    NewText string
}

// Plain text: source code, md, toml, yml, json, txt, csv, ini, latex, typst, rst, ...
type TextLocator struct {
    Path      string
    StartByte int
    EndByte   int
}

// Office Open XML (docx, xlsx, pptx). Uses stable IDs, not XPath indices.
type DocxLocator struct {
    Path        string
    ParagraphID string  // w14:paraId or injected bookmarkStart
    RunIndex    int     // index within the paragraph
    CharRange   [2]int  // chars within the run's concatenated text
}

type XlsxLocator struct {
    Path  string
    Sheet string
    Cell  string  // e.g. "B12"
}

type PptxLocator struct {
    Path       string
    SlideID    string
    ShapeID    string
    CharRange  [2]int
}

// Pandoc-mediated formats (epub, ipynb, mediawiki, dokuwiki, fb2, ...).
// Block-level pointer into the pandoc AST; needs OriginalContent on the node
// because pandoc round-trip is lossy.
type PandocLocator struct {
    Path     string
    Format   string // "epub", "ipynb", "mediawiki", ...
    ASTPath  []int  // indices into the Block list and its children
}
```

The graph stores `Locator` values on every node. Editing dispatches on the
concrete type via type switch.

---

## 4. Tier 1 — Plain Text Formats

Everything that is bytes-on-disk with offsets that mean what they look like:
source code, Markdown, TOML, YAML, JSON, INI, CSV, plain text, shell scripts, etc.

### Single edit

```go
func applyTextEdit(loc TextLocator, newText string) error {
    data, err := os.ReadFile(loc.Path)
    if err != nil { return err }
    out := make([]byte, 0, len(data)+len(newText)-(loc.EndByte-loc.StartByte))
    out = append(out, data[:loc.StartByte]...)
    out = append(out, newText...)
    out = append(out, data[loc.EndByte:]...)
    return atomicWrite(loc.Path, out)
}

func atomicWrite(path string, data []byte) error {
    dir := filepath.Dir(path)
    tmp, err := os.CreateTemp(dir, ".edit-*.tmp")
    if err != nil { return err }
    tmpName := tmp.Name()
    defer os.Remove(tmpName)
    if _, err := tmp.Write(data); err != nil { tmp.Close(); return err }
    if err := tmp.Close(); err != nil { return err }
    return os.Rename(tmpName, path)
}
```

### Multiple edits to the same file

Two viable approaches:

**A. Reverse-sorted edits on `[]byte`** — simpler, fine for tens of edits per file:

```go
// Sort edits descending by StartByte so earlier offsets stay valid.
sort.Slice(edits, func(i, j int) bool {
    return edits[i].StartByte > edits[j].StartByte
})
for _, e := range edits {
    data = append(data[:e.StartByte], append([]byte(e.NewText), data[e.EndByte:]...)...)
}
```

**B. Rope or piece table** — for many edits or large files. Why: inserting at
offset 500 of a 5MB `[]byte` shifts ~4.5MB per edit. A rope stores text as a tree
of chunks; insertions splice nodes, no big `memcpy`.

- `github.com/zyedidia/rope` — piece table, used by the `micro` editor.

Use a rope when a single user action produces many edits to one file (e.g., a
rename touching 50 call sites). Otherwise the reverse-sorted approach is plenty.

### Format-specific notes

- **Markdown** — parse with `github.com/yuin/goldmark` for structure, but **edit the raw bytes** using tree-sitter or goldmark ranges. Do not round-trip through AST-then-serialize; you'll lose formatting.
- **TOML** — for style-preserving edits use `github.com/pelletier/go-toml/v2`. `github.com/BurntSushi/toml` is fine for read-only decoding but doesn't preserve comments/whitespace.
- **YAML** — `gopkg.in/yaml.v3` preserves comments and key order. Same advice: locate via AST, edit bytes.
- **JSON** — stdlib `encoding/json` reorders keys and strips formatting. For style-preserving edits use `github.com/tidwall/sjson` (point-mutation by path) or treat as text and edit byte ranges from tree-sitter.

---

## 5. Tier 2 — Office Open XML (`.docx`, `.xlsx`, `.pptx`)

These are ZIP archives containing XML plus media/styles/relationships. You can't
edit by byte offset in the archive itself. The pattern is **unzip → edit XML →
rezip atomically**.

### Libraries

- `github.com/unidoc/unioffice` — most complete, covers all three formats with a paragraph/run object model. Commercial license for production use.
- `github.com/nguyenthenguyen/docx` — simpler, free, good for find/replace.
- `github.com/xuri/excelize/v2` — best-in-class for `.xlsx`, free, MIT, cell-coordinate API.
- **Full control path:** `archive/zip` (stdlib) + `github.com/beevik/etree` for XPath. More code but no license concerns and lets you keep formatting fidelity.

### `.docx` write path (XML approach)

```
1. archive/zip: open .docx, read all entries into memory
2. Locate word/document.xml
3. etree.Parse(...) → FindElement(loc.BodyPath) → splice text into <w:t>
4. zip.NewWriter onto a temp file, write all entries (modified document.xml plus untouched rest)
5. os.Rename temp over original
```

Gotchas:

- `<w:r>` (run) can contain multiple `<w:t>` (text) elements split by formatting marks like `<w:br/>` or `<w:tab/>`. For lossless edits, walk run children in order and only edit text nodes — don't collapse them.
- `xml:space="preserve"` is required on `<w:t>` if text has leading/trailing whitespace.
- Some readers care about entry order in the ZIP — preserve original order, and keep `[Content_Types].xml` first if you see issues.
- Compression method should be preserved per entry.

### `.xlsx` write path

Use `excelize` — cell-coordinate API is much more ergonomic than raw XML:

```go
f, err := excelize.OpenFile(loc.Path)
if err != nil { return err }
defer f.Close()
if err := f.SetCellValue(loc.Sheet, loc.Cell, newText); err != nil { return err }
return f.Save() // atomic via temp+rename internally
```

### `.pptx` write path

Same unzip/edit/rezip pattern as `.docx`. Each slide is `ppt/slides/slideN.xml`.
Text runs are `<a:r>` containing `<a:t>`, similar structure to Word.

### Stable locators across edits

XPath positions like `w:p[3]/w:r[2]` are fragile — insert a paragraph and all later
indices shift. Options:

1. **Re-resolve on every edit** by re-parsing after each save. Simple, slow.
2. **Inject stable IDs** by adding `<w:bookmarkStart>` markers or using Word's existing `w14:paraId` attributes.

For an interactive graph editor, option 2 is worth the complexity.

### Lost benefit: no incremental reparse

For Office formats, after any edit you must fully re-extract text and re-parse.
That's fine for human-speed editing, not for automated high-throughput edits.

---

## 5.5. Tier 3 — Pandoc-mediated Formats (EPUB, ipynb, MediaWiki, FB2, …)

For formats that don't have a tree-sitter grammar AND aren't OOXML containers,
the practical fallback is pandoc. Use pandoc to read the file into its JSON AST,
do semantic work on the AST, and write back via pandoc.

### Honest assessment of pandoc

- **No native Go port exists.** `gogap/go-pandoc` is an HTTP service wrapper around the `pandoc` binary; `nananas/go-pandocfilters` is a library for manipulating the JSON AST that pandoc emits. Both require the `pandoc` executable installed.
- **The pandoc AST is lossy.** From the pandoc docs: it preserves *structural elements* (paragraphs, headings, lists, basic tables) but not *formatting details* (margin sizes, custom styles, complex table layouts, embedded objects, comments, track changes unless explicitly flagged).
- **Round-tripping a file through pandoc will not produce bytewise-identical output.** This is fundamental, not a bug.

So pandoc is a great **read-only** semantic view, but unsuitable as a primary store
if you need to round-trip without loss.

### Strategy: lossy view + lossless edit-back

To get the best of both worlds, store both the pandoc AST subtree AND the
original source bytes on each node:

```go
node := &Node{
    Locator: PandocLocator{Path: "book.epub", Format: "epub", ASTPath: []int{3, 1, 2}},
    DisplayText: "Once upon a time, there was a graph...", // from pandoc AST
    Original: &OriginalContent{
        Bytes:  rawEpubChunkBytes, // the unmodified original HTML/XML fragment
        Format: "epub-html-fragment",
    },
}
```

When the user edits `DisplayText`:

1. Diff the new display text against the old display text → produces a minimal text-level change.
2. Apply that minimal change to the `Original.Bytes` (which is the source-format chunk) using a text-level splice. This preserves all surrounding markup that pandoc would have flattened.
3. Write the modified chunk back into the file.
4. Reparse via pandoc to refresh the AST.

This pattern works because most user edits are small text changes that don't
touch markup. For edits that DO touch markup (e.g., "make this bold"), you fall
back to AST-level edits and accept that pandoc will reserialize the surrounding
region, with the formatting-loss risk that implies. Document this clearly.

### Pipeline

```go
type PandocBridge struct {
    bin string // path to pandoc executable, or "" to use $PATH
}

func (p *PandocBridge) ToAST(path, format string) (json.RawMessage, error) {
    cmd := exec.Command(p.bin, "-f", format, "-t", "json", path)
    return cmd.Output()
}

func (p *PandocBridge) FromAST(ast json.RawMessage, format, outPath string) error {
    cmd := exec.Command(p.bin, "-f", "json", "-t", format, "-o", outPath)
    cmd.Stdin = bytes.NewReader(ast)
    return cmd.Run()
}
```

Subprocess is simpler than the `gogap/go-pandoc` HTTP service for most use cases.
Wrap calls in a worker pool to bound concurrency.

### Pandoc AST node shape (for reference)

The AST is a tree of `Block` and `Inline` elements:

```json
{
  "pandoc-api-version": [1, 23, 1],
  "meta": { "title": { "t": "MetaInlines", "c": [{"t":"Str","c":"My Book"}] } },
  "blocks": [
    { "t": "Header", "c": [1, ["chapter-1", [], []], [{"t":"Str","c":"Chapter 1"}]] },
    { "t": "Para",   "c": [{"t":"Str","c":"Once"}, {"t":"Space"}, {"t":"Str","c":"upon..."}] }
  ]
}
```

`PandocLocator.ASTPath` is a sequence of indices into this structure
(e.g., `[1, 0]` = first Inline of second Block).

---

## 6. The Edit Pipeline (End to End)

```go
type GraphEdit struct {
    NodeID  string
    NewText string
}

func (s *System) ApplyGraphEdit(ge GraphEdit) error {
    node, err := s.graph.Get(ge.NodeID)
    if err != nil { return err }

    // 1. Drift check
    if got := hashRegion(node.Locator); got != node.Hash {
        return s.rebaseOrReject(ge, node)
    }

    // 2. Compile to file edit and apply
    if err := node.Locator.Apply(Edit{NewText: ge.NewText}); err != nil {
        return err
    }

    // 3. Notify LSP — file content changed
    s.lsp.DidChange(node.Locator.Path(), readFile(node.Locator.Path()))

    // 4. Re-parse (incrementally where possible)
    if tl, ok := node.Locator.(TextLocator); ok {
        s.reparseIncremental(tl.Path, treeEditFromGraphEdit(ge, node))
    } else {
        s.reparseFull(node.Locator.Path())
    }

    // 5. Refresh affected graph nodes and edges
    s.refreshGraphFor(node.Locator.Path())
    return nil
}
```

### Incremental reparse (text formats only)

```go
tree.Edit(sitter.EditInput{
    StartIndex:  uint32(oldStart),
    OldEndIndex: uint32(oldEnd),
    NewEndIndex: uint32(oldStart + len(newText)),
    StartPoint:  oldStartPt,
    OldEndPoint: oldEndPt,
    NewEndPoint: newEndPt,
})
newTree, _ := parser.ParseCtx(ctx, tree, newSource)
changed := newTree.ChangedRanges(tree)
```

Use `changed` to find which graph nodes overlap the changed ranges and refresh
just those.

---

## 7. Concurrency & Atomicity

- **One writer per file at a time.** A per-path `sync.Mutex` keyed by absolute path is sufficient for a single process.
- **Atomic writes everywhere.** Always write to a temp file in the same directory, then `os.Rename`. On POSIX this is atomic; on Windows it's atomic if the destination exists. (For Windows + new file creation, the rename can fail if the destination is open elsewhere — handle the error.)
- **fsync the temp file before rename** if durability against crash matters: `tmp.Sync()` before `tmp.Close()`.
- **LSP `didChange` ordering** — send the notification *after* the file is written, not before. Some servers re-read from disk on certain events.

---

## 8. Suggested Module Layout

```
/cmd/baged/                  # daemon entry point (Båge daemon)
/internal/graph/             # graph data model, storage, queries
/internal/parse/treesitter/  # tree-sitter wrappers + grammars
/internal/parse/lsp/         # LSP client, server lifecycle
/internal/parse/pandoc/      # pandoc subprocess bridge + AST helpers
/internal/edit/              # Locator interface + per-format implementations
    text.go                  # Tier 1
    docx.go                  # Tier 2
    xlsx.go
    pptx.go
    pandoc.go                # Tier 3
/internal/edit/rope/         # optional: rope-backed buffer
/internal/atomic/            # atomic write helper
/pkg/api/                    # public types
```

---

## 9. Dependency Cheat Sheet

| Concern              | Library                                              |
|----------------------|------------------------------------------------------|
| File I/O             | stdlib `os`, `io`                                    |
| ZIP (docx/xlsx/pptx) | stdlib `archive/zip`                                 |
| XML                  | stdlib `encoding/xml`, `github.com/beevik/etree`     |
| Tree-sitter          | `github.com/tree-sitter/go-tree-sitter`              |
| LSP protocol         | `go.lsp.dev/protocol`                                |
| LSP transport        | `go.lsp.dev/jsonrpc2`                                |
| Rope buffer          | `github.com/zyedidia/rope`                           |
| TOML (style-preserving) | `github.com/pelletier/go-toml/v2`                 |
| YAML                 | `gopkg.in/yaml.v3`                                   |
| JSON point-edit      | `github.com/tidwall/sjson`                           |
| Markdown AST         | `github.com/yuin/goldmark`                           |
| `.xlsx` high-level   | `github.com/xuri/excelize/v2`                        |
| `.docx`/`.pptx` high-level | `github.com/unidoc/unioffice` (commercial) or roll your own via etree |
| Pandoc bridge        | `os/exec` subprocess wrapping the `pandoc` binary (or `gogap/go-pandoc` HTTP service) |
| Pandoc filter helpers | `github.com/oltolm/go-pandocfilters` or `github.com/nananas/go-pandocfilters` |

### Tier summary

| Tier | Formats                                                  | Parser                   | Locator type     | Recreation info on node |
|------|----------------------------------------------------------|--------------------------|------------------|--------------------------|
| 1    | source code, md, toml, yml, json, latex, typst, rst, asciidoc, org, rtf, xml | tree-sitter | `TextLocator`    | No (file is truth)       |
| 2    | docx, xlsx, pptx                                         | unioffice / excelize / etree | `DocxLocator` / `XlsxLocator` / `PptxLocator` | Optional (offline/rollback) |
| 3    | epub, ipynb, mediawiki, dokuwiki, fb2, opml, …           | pandoc subprocess        | `PandocLocator`  | **Yes** (pandoc is lossy) |
| —    | pdf                                                      | read-only (pikepdf-equiv via subprocess) | n/a   | n/a, no write-back       |

---

## 10. Open Questions Worth Resolving Early

1. **UTF-16 vs UTF-8 in LSP ranges.** LSP positions are UTF-16 code units. Tree-sitter is byte/UTF-8. Decide where you convert and centralize it.
2. **Stable node identity across reparses.** Tree-sitter nodes are not stable — they're rebuilt on every parse. Assign your own IDs based on stable identifiers (qualified symbol names from the LSP plus a structural path within the file). For OOXML, lean on `w14:paraId` or inject bookmark IDs.
3. **Backpressure on the LSP.** Don't fire `didChange` faster than the server can keep up; debounce per file.
4. **PDF intentionally out of scope** for write-back. Treat as read-only nodes in the graph.
5. **Pandoc as a hard dependency.** Tier 3 requires the `pandoc` binary at runtime. Decide whether to bundle it, document it as a prereq, or make Tier 3 opt-in. Subprocess startup is ~50ms so for high-throughput Tier 3 work, consider a long-lived `pandoc server` process or batch invocations.
6. **Recreation info policy.** Default is: don't store original bytes on Tier 1/2 nodes; do store them on Tier 3. Revisit if you add offline editing or per-node history.

---

## 11. Prior Art

- **Sourcegraph SCIP / LSIF** — code graph indexes (read-only).
- **Glean** (Meta) — code knowledge graph.
- **Stack Graphs** (GitHub) — incremental name resolution.
- **gopls' internal architecture** — example of LSP server using tree-sitter-like incremental analysis.
- **Cursor, Aider** — LLM-driven code edits expressed as LSP-style ranges.
