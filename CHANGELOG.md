# Changelog

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
