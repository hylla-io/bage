# Changelog

## 0.15.0 — one parse per file, positions without text scans, `_` names nothing, open once (source breaks)

`Block`, every fact and scope shape, and `region_hash` are unchanged. **Three source breaks for a
v0.14.0 caller**, each needing nothing unless the caller builds or exhaustively matches the type.

### Source breaks, and the fix for each

- **`inspect::Symbol` has three new fields**: `start_point`, `end_point`, `name_range`. A struct
  literal adds them.
- **`lsp::ProbeAnswer` has a new variant**, `SameFile { locations }`. An exhaustive match adds it.
- **`lsp::LspError` has three new variants**: `ReadyProbesDeadline`, `NoReadyProbe`,
  `BlankProbe`. An exhaustive match adds them.

### Added

- `inspect::read_placed_blocks` and `PlacedBlock`: each block with tree-sitter's start and end
  points and the byte range of its name node.
- `inspect::open_bytes`: the one parse a file needs, from bytes the caller holds.
- `region::LineIndex::point_for_byte`: a zero-based point by binary search.
- `lsp::TextIndex`: one file indexed for LSP positions; `position_at` and `byte_offset` by binary
  search, agreeing exactly with the walking functions of the same names.
- `lsp::Client::ensure_open`, and `Client::await_ready_by` with `ReadyProbe`: ready only on a
  definition in another file, or on any location while every declared ready signal holds; a
  candidate on `_` is refused before anything is sent.

### Behaviour changes

- **`extract_facts` and `extract_scopes` parse nothing**: they read the tree the file was opened
  with (a tree without one is still parsed once more). Their queries compile once per process and
  language, and a language's whole-file facts run as one query. Scopes no longer ask the engine
  for parents, which it finds by searching down from the root.
- **`_` names nothing in Rust and Go**: never an import name, binding or occurrence. A Rust
  lifetime or loop label is never an occurrence.
- **A query no longer re-opens its document.** Every query, `rename` and its priming open a
  document only when the server does not already hold it with the same text; `diagnostics` still
  re-opens, because it needs a fresh publish.
- **`reparse` places an edit from one line table** instead of three scans of the text.
- **The tree-sitter DTO is built with a cursor**, linear in a node's width.
- **Test builds are optimised** (`[profile.test] opt-level = 1`, dependencies at 2), so the timed
  tests measure bage rather than the build mode.

## 0.14.0 — LSP: kept diagnostics, failure rules, hover (source breaks)

**Every change is in `bage::lsp`** (SPEC §12.6). Parsing, names and `region_hash` are unchanged.
The CLI's source is unchanged too; its sessions run through `LspPool`, so they now declare the
hover capability and leave out a `null` params. **Five source breaks for a v0.13.0 caller**, each
with a one-line fix, and four behaviour changes, all listed below.

### Source breaks, and the fix for each

- **`LspError::ReadyDeadline` is removed.** Match `LspError::ReadySignalDeadline { .. } |
  LspError::ReadyProbeDeadline { .. }` instead. `last` is now a `ProbeAnswer`, not a `String`:
  compare with `ProbeAnswer::Empty`, or print it with `last.to_string()`.
- **`LspError::ServerReported` has a new field, `locations`.** In a pattern that names every
  field, add `locations` or `..`.
- **`ClientConfig` has two new fields.** In a full literal add `diagnostic_failures: Vec::new()`
  and `hover_content_format: vec![MarkupKind::Markdown, MarkupKind::PlainText]`. A literal ending
  in `..ClientConfig::default()` needs nothing.
- **`ReadySignal` has a new field, `mode`.** Add `mode: ReadyMode::Latest`, which is what v0.13.0
  did.
- **`ReadyFailure` has a new field, `mode`.** Add `mode: ReadyMode::Latest`, likewise.

### Behaviour changes

- **`initialize` now declares a hover capability**: `textDocument.hover.contentFormat` of
  `["markdown", "plaintext"]`. v0.13.0 sent none. `hover_content_format: Vec::new()` omits it
  again.
- **A rule on `textDocument/publishDiagnostics` is now observed.** v0.13.0 never showed that
  method to a `ReadyFailure` or `ReadySignal`, so such a rule could not match. It now matches as
  on any other method, and can end `await_ready`. Prefer a `DiagnosticFailure`: a whole-publish
  match must equal a document's whole list. Under `Latest` it sees only the last document
  published; under `Once` any publish that matches counts.
- **A `null` params is no longer sent.** v0.13.0 wrote `"params": null`; the member is now left
  out. It reaches `shutdown` and `exit` in `Client::close`, and the `$/bage/barrier` request in
  `Client::diagnostics`. **Why**: JSON-RPC allows `params` only as an object or an array, and
  TypeScript's server rejects `"params": null` on `shutdown` and `exit` and then never exits. No
  in-process test asserts the member is absent.
- **Error text changed** with the types.
  - A probe deadline prints `unresolved after` (was `still empty after`).
  - Its `last` text: a refusal prints `refused: <message>` (was `lsp: textDocument/definition:
    <message>`); a timeout prints `no response after <duration>`, without that prefix.
  - A probe deadline no longer carries a `<method> has not reported <when>` sentence. That case
    is the signal deadline, which prints `<method> did not report <when> within <deadline>;
    latest: …`.
  - `ServerReported` prints one `at path:line:col: message [source code]` line per location.

### Ready signals: a state or an event

- **`ReadyMode::Latest`** (`"latest"`, the default) reads a STATE: the rule holds only while the
  latest notification of its method matches. This is v0.13.0's behaviour.
- **`ReadyMode::Once`** (`"once"`) reads an EVENT: any matching notification holds the rule for
  good, whatever that method carries afterwards.
- **Why**: gopls announces a finished load with ONE `window/showMessage` of `Finished loading
  packages.`, then sends unrelated messages on that method. Read as a state, the signal is
  withdrawn by the next message and a loaded server times out.
- A `Once` signal stays met for as long as that exact declaration is made, across
  `Client::configure` calls. A changed declaration starts over from the latest notification kept
  for its method: it is met at once when that one matches.
- `ReadyFailure` takes the same `mode`. Under `Once` the FIRST matching notification is the one
  reported, and it is kept for as long as that exact declaration is made.
- **A new or changed `Once` `ReadyFailure` starts with nothing**, unlike a signal: the
  notification kept for its method is not read, so only one that arrives after the declaration
  counts.
- **A new or changed `Once` `DiagnosticFailure` does read what is held**: a diagnostic the server
  currently holds counts at once when it matches.
- `ReadySignal` and `ReadyFailure` now read and write as data (a signal is `{"method", "when",
  "mode"}`). An unknown member or mode name is refused, never read as `latest`.

### Diagnostics are kept, and can be read

- **Every publish is stored per document.** A later publish for a document replaces its list; an
  empty one clears it. v0.13.0 handed a publish only to a waiting `Client::diagnostics` call.
- **`Client::published_diagnostics()`** reads every document's current list as
  `PublishedDiagnostic { uri, path, start_line, start_char, end_line, end_char, severity, source,
  code, message, raw }`. Positions are zero-based UTF-16. `path` is relative to the root when the
  document lies under it, absolute otherwise. `raw` is the diagnostic as the server sent it.
- **`severity` is an `Option<Severity>`**: `Error`, `Warning`, `Information`, `Hint` (LSP 1 to
  4), or `Other(i64)` for a number the protocol does not define. `None` when the server sent
  none.
- **`Client::pull_diagnostics(path, content) -> Result<Vec<PublishedDiagnostic>, LspError>`**
  asks with `textDocument/diagnostic`, returns that document's diagnostics and stores them the
  same way. A server without `diagnosticProvider` is refused with `Unsupported`. No test covers
  this method yet.
- The store holds one entry per document that currently has a diagnostic, for the client's life.
- `Client::diagnostics` and its `Diagnostic` shape are unchanged.

### Diagnostic failure rules

- **`DiagnosticFailure { when, file_name, mode }`** in `ClientConfig::diagnostic_failures`
  declares which ONE diagnostic means the project did not load.
  - `when` is matched against the diagnostic as sent: `{"source": "go list"}`, `{"severity": 1}`.
  - `file_name` is an optional pattern on the last path segment, `*` being any run of
    characters: `"go.mod"`, `"*.go"`.
  - `mode`: `Latest` counts only diagnostics the server still holds; `Once` keeps counting one
    the server withdrew.
- As data it is `{"when", "file_name", "mode"}`; an unknown member is refused.
- **Bage picks no diagnostic.** With none declared, none stops anything.
- A rule that holds ends `await_ready` at once with `ServerReported`.

### `ServerReported` says where

- **`locations: Vec<PublishedDiagnostic>`** lists every diagnostic a declared rule selected.
- It is empty when only a notification rule held, or when the server located nothing in a
  diagnostic.
- When only a diagnostic rule held, `method` is `PUBLISH_DIAGNOSTICS_METHOD` and `message` is the
  first location's.

### Two deadline errors instead of one

- **`ReadySignalDeadline { signal, latest, after, stderr }`**: a declared signal never held.
  `latest` is the last params seen for its method, `None` when the server never sent it.
  `signal` is a `Box<ReadySignal>` and `latest` an `Option<Box<Value>>`.
- **`ReadyProbeDeadline { path, line, character, after, last, stderr }`**: every signal held and
  the probe never resolved. `last` is `ProbeAnswer::Empty`, `Refused { message }` or
  `NoResponse { after }`.
- **Why**: a caller reads which half never came without parsing a sentence.

### `Client::check_failures()`

- Judges the declared `ready_failures` and `diagnostic_failures` against everything received SO
  FAR. Sends nothing, waits for nothing.
- **For use after queries**: a server keeps reporting once it has answered.
- **`Ok` means no declared failure has ARRIVED, never that none will.** A caller that must cover
  a server's delay waits that long before asking.

### `Client::hover`

- **`Client::hover(path, content, line, character) -> Result<Option<Hover>>`**: what the server
  shows for the symbol at a position, a symbol defined outside the workspace included.
- **`Hover { text, kind, range }`**: the server's text, the `MarkupKind` (`Markdown` or
  `PlainText`) the SERVER says it is in, and the span hovered when the server names one.
- `None` is a `null` answer or empty content. It carries no readiness information: gate with
  `await_ready` first.
- A server without `hoverProvider` is refused with `Unsupported`, nothing sent.
- **`ClientConfig::hover_content_format`** is the declared format list, most wanted first.
  Markdown leads by default: it is the one format in which every server measured keeps the
  signature apart from the documentation.
- **`Client::hover_content_format`** is a new public field holding the list in force.
- **`MarkupKind` reads and writes as data** as `"markdown"` and `"plaintext"`.
- **A position outside the text costs the whole `query_deadline`** (30 s by default) on a server
  that refuses it, and ends as `QueryDeadline`. `Client::definition` behaves the same. Bage
  checks no position against the text.
- **A server shows only documentation it can read.** rust-analyzer needs the toolchain's
  `rust-src` for the standard library; pyright needs a Python interpreter on `PATH` for
  docstrings.

**Measured** against real servers (`tests/lsp_session.rs`, `BAGE_LSP_REAL_TEST=1`):

- **gopls, `Once` signal**: holds through the messages that follow `Finished loading packages.`;
  the same signal read as `Latest` is the control.
- **gopls, `go.mod` with an unclosed `require (`**: reported at `go.mod` 5:0, source `syntax`.
  Its `go list` diagnostic on the opened source file arrives about a second after the gate and
  is found by `check_failures`. Undeclared, the same module waits out the deadline.
- **rust-analyzer, `Cargo.toml` that does not parse**: publishes NO diagnostic. It reports
  `Failed to load workspaces.` by status notification; the position exists only as cargo's text
  in `stderr`, so `locations` is empty.
- **Hover**: rust-analyzer on `HashMap::insert` and a vendored registry crate, with the
  no-sources control; gopls on `fmt.Println`; `tsc --lsp` on `parseInt` and a `node_modules`
  package; pyright on `json.dumps`.
- **Positions outside the text**, at a 1 s deadline: gopls refuses a column past its line and a
  line past the file; rust-analyzer refuses the line and answers the column with `None`.

## 0.13.0 — expression and impl names (contract change)

**A CONTRACT change to `Symbol.name` / `Block.name`** (SPEC §9.4, HYLLA_NODE_CONTRACT §1a) for
two kinds of block. Kinds, byte ranges, lines and `region_hash` are unchanged. A host that
builds ids from names (Hylla) sees these nodes renamed. Misread items keep their v0.11.0 names,
as in 0.12.0.

### TypeScript / TSX / JavaScript: function and class expressions

- **Named by the binding that holds them**, never by a word from their parameters or body:
  `const f = () => …` → `f`; `this.parseArg = (arg) => …` → `parseArg`;
  `obj.x = function () {}` → `x`; `{ onload: () => … }` → `onload`; `onClick = () => …` in a
  class → `onClick`; `const Model = class {}` → `Model`.
- **A callback is unnamed** (`""`), as are default values and computed targets. A function or
  class that spells its own name keeps it (`function helper() {}`).
- **Before**, an arrow took the first identifier in its parameters or body: `(node) =>
  node.visible` was named `node`.

### Rust: impl blocks

- **Named by the type**, not the trait: `impl BlobRef` → `BlobRef`, `impl<T> Wrapper<T>` →
  `Wrapper<T>`.
- **A trait impl is `<Type as Trait>`**: `impl Validate for BlobRef` → `<BlobRef as Validate>`.
  So `fmt` under `Display` and `fmt` under `Debug` sit under differently named impls.
- **Before**, a trait impl took the trait's name (`Validate`), and `impl<T> W<T>` could take `T`.

### Languages

- **Hylla's MVP code languages are now Rust, TypeScript/TSX, JavaScript, Python and Go.** Their
  outline names are verified against each language's server.
- **C, C++ and C# leave Hylla's MVP.** Båge still ships and parses every grammar.
- **Known unreliable:** C and C++ names on macro-heavy or misread code (C++ in a `.h` is read
  as C). C# names are lightly measured.

**Measured** on Hylla's own tree (`app/src` TS/TSX, `crates/**/*.rs`), old rule vs new, each
block compared with a reference namer:

| units | blocks | old matches reference | new matches reference |
| --- | ---: | ---: | ---: |
| TS/JS arrow functions | 11,339 | 7,400 | 11,280 |
| — of those the reference names | 2,086 | 16 | 2,042 |
| TS/JS class expressions | 38 | 12 | 34 |
| TS/JS function expressions | 32 | 29 | 30 |
| Rust impls | 1,073 | 609 | 1,073 |

- **Reference, TS/JS:** the TypeScript 5.9.3 compiler's `getNameOfDeclaration`.
- **Reference, Rust:** `rust-analyzer symbols` labels (`impl T for X` read as `<X as T>`).
- **The remaining TS/JS differences are by rule:** 35 string keys that are not plain identifiers
  stay quoted (`"tab.next"`); class-field and member-assigned expressions are named where
  TypeScript's compiler leaves them unnamed; 13 arrows sit in misread items and keep old names.

## 0.12.0 — outline names (contract change)

**Every change below is a CONTRACT change to `Symbol.name` / `Block.name`** (SPEC §9.4,
HYLLA_NODE_CONTRACT §1a). Block kinds, byte ranges, lines and `region_hash` are unchanged: an
old-vs-new run over the corpora below found zero structural differences. A host that builds ids
from names (Hylla) sees these nodes renamed.

### The rule: new names on a clean parse, v0.11.0 names on a misread

- **A block is named by the new field rules only when the top-level item holding it contains
  no ERROR or MISSING node, and the root itself is not an ERROR.** Every code grammar, one
  rule. An ERROR root (aws-lc `include/openssl/pool.h`) leaves every item in the file misread,
  however clean its own nodes.
- **A block in a misread item keeps exactly the name v0.11.0 gave it**: its first
  identifier-kind child, raw. That name may be a keyword, a type, a macro, or several lines with
  comments in them, as it was in v0.11.0.
- **Why**: the grammar cannot expand macros and reads every `.h` as C. Inside a misread, its
  fields hold whatever word landed there: a keyword, a macro, a parameter, a callee. Two rounds
  of per-shape recovery rules each fixed the cases they were written for and broke code nobody
  had read (the second lost the names of 6,821 macro-wrapped C declarations). Keeping v0.11.0's
  name changes nothing a host already relied on.
- **The rule is coarse by design**: one ERROR anywhere in an include-guarded header or an
  `extern "C" {` block keeps every declaration in it on the v0.11.0 name, including sound ones
  and including the return-type names this change fixes elsewhere. 64% of the blocks measured
  below sit in misread items; most are C and C++ headers.

**Measured** by running `bage show` from v0.11.0 and from this change over the same files and
comparing every block's name. "Misread" is decided by a separate tree-sitter probe with the
same grammar versions, not by bage.

| corpus | files | blocks | in misread items | renamed |
| --- | ---: | ---: | ---: | ---: |
| aws-lc (C, C++, `.inc`) | 866 | 96,014 | 30,955 | 16,795 |
| libFuzzer (C++, headers read as C) | 53 | 4,816 | 2,540 | 497 |
| jemalloc C++ shims, vswhom (C++) | 9 | 658 | 312 | 64 |
| ring (C) | 46 | 6,745 | 4,655 | 463 |
| QuickJS (C) | 40 | 18,333 | 4,996 | 4,190 |
| mimalloc (C) | 78 | 14,010 | 8,961 | 965 |
| tree-sitter runtime (C) | 75 | 5,236 | 2,220 | 994 |
| libsqlite3-sys, SQLite amalgamation (C) | 9 | 87,385 | 86,042 | 1,158 |
| `/opt/homebrew/include` (C, C++) | 4,779 | 596,358 | 537,050 | 15,223 |
| jsdom `lib` (JavaScript) | 652 | 28,505 | 0 | 1,950 |
| xterm `src` + typings, zod 4 `src` (TypeScript) | 439 | 24,010 | 95 | 1,099 |
| TypeScript `lib.*.d.ts`, `@types/node` (TypeScript) | 187 | 22,396 | 833 | 4,073 |
| Hylla `app/src` (TypeScript, TSX) | 724 | 27,009 | 51 | 227 |
| Hylla `crates` + `xtask` (Rust) | 700 | 114,256 | 15 | 0 |
| bubbletea, lipgloss, glamour, anthropic-sdk-go (Go) | 199 | 15,040 | 0 | 0 |
| pip (Python) | 404 | 7,332 | 0 | 913 |
| tree-sitter-c-sharp tests (C#) | 6 | 188 | 0 | 28 |
| Hylla unit-kind fixtures (all grammars) | 21 | 468 | 0 | 55 |

- **Blocks in misread items renamed: 0**, in every corpus (678,725 blocks).
- **Every rename is on a clean parse** (48,694). Each is one of: the declarator instead of the
  return or field type, the first declarator of several, a name kept whole (qualified, template,
  destructor, operator), a member key, a body left unnamed, a construct signature, a decorated
  definition, or a keyword, macro or type dropped (below).
- **No clean-parse name holds a newline or a comment.** The 842 misread names that do are
  v0.11.0's, unchanged.
- **No clean-parse name was changed to a keyword or builtin type.** 606 clean names are one,
  unchanged from v0.11.0, where the source spells it so: `static_cast<…>` and
  `reinterpret_cast<…>` read as `template_function` nodes, and `#define alignas(x)`.

### Rust, Go

- No name changes.

### C (clean parses)

- A declaration is named after its DECLARATOR, never its return or field type. Before, the first
  identifier won, which in C is the type.
  - `declaration` (13,193 renamed): `CBB cbb;` was `CBB`, now `cbb`; `uint8_t *p = …;` was
    `""`, now `p`; `char *section = NULL, *buf;` was `buf`, now `section` (the first
    declarator).
  - `field_declaration` (12,175): `BIO *bio;` was `BIO`, now `bio`; a function-pointer field was
    `""`, now named (`callback`).
  - `function_definition` (2,470): `ASN1_INTEGER *ASN1_INTEGER_dup(…)` was `ASN1_INTEGER`, now
    `ASN1_INTEGER_dup`; a function returning a pointer was `""`, now named.
  - `function_declarator` (1,121): named after its declarator; most were `""`.
  - `type_definition` (421): `typedef __m128i aes_word_t;` was `__m128i`, now `aes_word_t`;
    `typedef _Bool bool;` was `_Bool`, now `bool`; a function-pointer typedef was `""`, now
    named.
- An MSVC calling convention is skipped: `static void __cdecl do_library_init(void);` (aws-lc
  `crypto/crypto.c:52`) is `do_library_init`.
- A name is one line: whitespace runs collapse to one space, and a comment is never part of a
  name.
- **Code the grammar reads without an ERROR but cannot expand** (a macro, C++ in a `.h`) still
  reaches these rules. Three shapes are handled:
  - a keyword or builtin type in the declarator slot is no name: `SET_DIT_AUTO_RESET if (…) {…}`
    read as a definition is `""` (was `SET_DIT_AUTO_RESET`); a C++ `float operator()(…)` in a
    `.h` is `""` (was `operator`, or the Metal qualifier `thread`). C also refuses `operator`,
    at the cost of a C variable literally named `operator`;
  - a keyword that names no type in the type slot declares nothing: `export C_LIB_NAMESPACE
    {…}` is `""` (was `export`);
  - a definition whose declarator is bare parentheses is named after the type word:
    `do_library_init(void) {…}`, its macro return type on the line before, stays
    `do_library_init`.
  - 137 clean blocks named in v0.11.0 are now `""` this way. Each v0.11.0 name was a keyword,
    macro or type: `export` 34, `operator` 34, `thread` 55, `BSSL_NAMESPACE_BEGIN` 6, `U` 2,
    and one each of `SET_DIT_AUTO_RESET`, `if`, `float`, `complex64_t`, `CONSTEXPR_ARRAY`,
    `ISL_MAYBE`.

### C++ (clean parses)

- Everything under C, plus:
  - `declaration` (8,250), `field_declaration` (1,139), `function_definition` (845):
    `std::vector<uint8_t> der;` was `std::vector<uint8_t>`, now `der`;
    `bssl::UniquePtr<BIGNUM> BIGNUMPow2()` now `BIGNUMPow2`.
  - `function_declarator` (251): a destructor keeps its `~`: was `OwnedSocket`, now
    `~OwnedSocket`.
  - `template_declaration` (135): named after what it holds (`template <…> T twice(T)` was `T`,
    now `twice`).
  - `friend_declaration` (13): named after what it befriends (`friend void swap(A&, A&);` was
    `""`, now `swap`; `friend class Vec<int>;` is `Vec<int>`).
- Names kept whole that v0.11.0 cut at the first identifier:
  - `namespace a::b::c {…}` was `a`, now `a::b::c`;
  - a template specialization `template <> struct Box<int> {…}` was `Box` (and its
    `template_declaration` `""`), now `Box<int>`;
  - template ids and structured bindings: `f<int>`, `m<int>`, `[a, b]`.
- **Operator names may contain `/`**: `Frac operator/(Frac b) const;` was `Frac`, now
  `operator/`; `operator/=` likewise. A host that builds paths from names must escape it.
- Conversion operators keep their target type and drop their parameters and qualifiers:
  `operator const char *() const` is `operator const char *`, `operator int &()` is
  `operator int &` (distinct from `operator int`), `ns::Widget::operator bool() const` is
  `ns::Widget::operator bool`; one written across lines is one line.

### C#

- `variable_declaration` (18), `field_declaration` (8): the first declarator, not the type
  (`Int64 D_e_f;` was `Int64`, now `D_e_f`).
- `accessor_declaration` (9): the accessor keyword (`get`, `set`, `init`, `add`, `remove`).
- `destructor_declaration`: `~Class` (was `Class`). `operator_declaration`: `operator +`
  (was `""`). `event_field_declaration`: the event name (was `""`).
- `namespace_declaration`: the qualified name kept whole (`Sample.Shapes`, was `Sample`).

### Python

- `decorated_definition` (914): named after the function or class it decorates, never the
  decorator (`@classmethod def find_spec` was `classmethod`, now `find_spec`).

### JavaScript

- `class_body` (414): `""`. It was named after the class's first member (usually
  `constructor`), so every member id nested under it carried that name.
- `method_definition` (1,539), and `field_definition`: a string, number or computed key is the
  name.
  - It loses its brackets and quotes only when a plain ASCII identifier or a number is left:
    `['KEY']` → `KEY`, `[42]` → `42`, `["field"]` → `field`.
  - Every other key stays exactly as written: `"-webkit-align-content"`,
    `[Symbol.asyncIterator]`, `[idlUtils.namedGet]`, `['a/b']`, `[KEY]`, and a computed key
    holding a comment beside its literal (`['k' /* c */]`).
  - Before, such a member was `""`, or took its first PARAMETER's name (a setter
    `set "-webkit-align-content"(V)` was `V`).
  - In jsdom and xterm no key reduced to a plain identifier; all 1,545 renamed members kept
    their key as written.

### TypeScript, TSX

- `class_body` (464), `interface_body` (3,486), `enum_body` (80): `""`, never the first member's
  name.
- `method_definition`, `public_field_definition`, `method_signature`,
  `abstract_method_signature`, `enum_assignment`: the JavaScript key rule above
  (`[Symbol.iterator]` was `""`, now `[Symbol.iterator]`).
- `construct_signature` (1,087): `new()`, as TypeScript's own navigation tree names it. It was
  named after its return type (`new (): AbortController` was `AbortController`), or `""` when
  that type was generic.
- `module` (121) with a string name is named as written: `declare module '@xterm/xterm'` was
  `""`, now `'@xterm/xterm'`.
- A file with a syntax error keeps v0.11.0's names in the item holding it, bodies and construct
  signatures included.

### Java and Ruby (parsed by Båge; not in Hylla's MVP)

- Java: `class_body`, `interface_body`, `enum_body`, `enum_body_declarations` are `""`;
  `field_declaration` and `method_declaration` are named after the declarator, not the type
  (`String name()` was `String`); `method_invocation` is the invoked method (`shape.area()`
  was `shape`, now `area`).
- Ruby: `class` and `module` are named (was `""`).

### Bash

- `function_definition` is named (was `""`).
