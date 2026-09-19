//! `textDocument/implementation` and the type hierarchy against REAL language
//! servers, through the crate's public API only.
//!
//! Runs only with `BAGE_LSP_REAL_TEST=1` and prints a loud SKIP line
//! otherwise; under the opt-in a missing server FAILS rather than skipping,
//! because the operator asked for that tier.
//!
//! Which server advertises what is itself asserted: a server that does not
//! advertise a capability must yield the typed `Unsupported`, and a server
//! that starts advertising one fails its case loudly so it moves to the
//! positive rows instead of silently losing coverage.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bage::lsp::{self, Client, ClientConfig, LspError, SymbolLocation, TypeTarget};

/// A position named by the first occurrence of `needle` on zero-based `line`.
#[derive(Clone, Copy)]
struct At {
    file: &'static str,
    line: u32,
    needle: &'static str,
}

enum Implementation {
    /// Querying `at` must return a location in `want_file`.
    Finds { at: At, want_file: &'static str },
    /// The server does not advertise `implementationProvider`.
    Unsupported { at: At },
}

enum TypeHierarchy {
    /// `sub`'s supertypes include `sup`, and `sup`'s subtypes include `sub`.
    Pair {
        sub: At,
        sub_name: &'static str,
        sup: At,
        sup_name: &'static str,
    },
    /// The server does not advertise `typeHierarchyProvider`.
    Unsupported { at: At },
}

struct Case {
    name: &'static str,
    server: &'static [&'static str],
    files: &'static [(&'static str, &'static str)],
    /// A reference that must resolve once the server is ready.
    probe: Option<At>,
    implementation: Option<Implementation>,
    type_hierarchy: Option<TypeHierarchy>,
}

const CASES: &[Case] = &[
    Case {
        name: "rust-analyzer",
        server: &["rust-analyzer"],
        files: &[
            (
                "Cargo.toml",
                "[package]\nname = \"t\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", "pub mod shape;\npub mod square;\n"),
            (
                "src/shape.rs",
                "pub trait Shape {\n    fn area(&self) -> f64;\n}\n",
            ),
            (
                "src/square.rs",
                "use crate::shape::Shape;\n\npub struct Square(pub f64);\n\nimpl Shape for Square {\n    fn area(&self) -> f64 {\n        self.0 * self.0\n    }\n}\n",
            ),
        ],
        probe: Some(At {
            file: "src/square.rs",
            line: 4,
            needle: "Shape",
        }),
        implementation: Some(Implementation::Finds {
            at: At {
                file: "src/shape.rs",
                line: 1,
                needle: "area",
            },
            want_file: "src/square.rs",
        }),
        // rust-analyzer advertises no `typeHierarchyProvider`.
        type_hierarchy: Some(TypeHierarchy::Unsupported {
            at: At {
                file: "src/shape.rs",
                line: 0,
                needle: "Shape",
            },
        }),
    },
    Case {
        name: "gopls",
        server: &["gopls"],
        files: &[
            ("go.mod", "module example.com/t\n\ngo 1.21\n"),
            (
                "shape.go",
                "package t\n\ntype Shape interface {\n\tArea() float64\n}\n",
            ),
            (
                "square.go",
                "package t\n\ntype Square struct{ s float64 }\n\nfunc (q Square) Area() float64 { return q.s * q.s }\n\nvar _ Shape = Square{}\n",
            ),
        ],
        probe: Some(At {
            file: "square.go",
            line: 6,
            needle: "Shape",
        }),
        implementation: Some(Implementation::Finds {
            at: At {
                file: "shape.go",
                line: 3,
                needle: "Area",
            },
            want_file: "square.go",
        }),
        type_hierarchy: Some(TypeHierarchy::Pair {
            sub: At {
                file: "square.go",
                line: 2,
                needle: "Square",
            },
            sub_name: "Square",
            sup: At {
                file: "shape.go",
                line: 2,
                needle: "Shape",
            },
            sup_name: "Shape",
        }),
    },
    Case {
        name: "clangd",
        server: &["clangd"],
        files: &[(
            "main.cpp",
            "struct Base {\n  virtual ~Base();\n};\n\nstruct Derived : Base {};\n\nint main() { Derived d; return 0; }\n",
        )],
        probe: Some(At {
            file: "main.cpp",
            line: 6,
            needle: "Derived",
        }),
        implementation: None,
        type_hierarchy: Some(TypeHierarchy::Pair {
            sub: At {
                file: "main.cpp",
                line: 4,
                needle: "Derived",
            },
            sub_name: "Derived",
            sup: At {
                file: "main.cpp",
                line: 0,
                needle: "Base",
            },
            sup_name: "Base",
        }),
    },
    Case {
        name: "pyright",
        server: &["pyright-langserver", "--stdio"],
        files: &[("a.py", "class A:\n    def f(self) -> int: ...\n")],
        probe: None,
        // pyright advertises no `implementationProvider`.
        implementation: Some(Implementation::Unsupported {
            at: At {
                file: "a.py",
                line: 1,
                needle: "f",
            },
        }),
        type_hierarchy: Some(TypeHierarchy::Unsupported {
            at: At {
                file: "a.py",
                line: 0,
                needle: "A",
            },
        }),
    },
];

/// The file bytes, path and UTF-16 position of `at` (fixtures are ASCII, so
/// the byte column is the UTF-16 column).
fn resolve(root: &Path, at: At) -> (String, String, u32, u32) {
    let path = root.join(at.file);
    let content = fs::read_to_string(&path).expect("read fixture");
    let text = content.lines().nth(at.line as usize).expect("line");
    let col = text.find(at.needle).expect("needle on line") as u32;
    (
        path.to_str().expect("utf-8").to_string(),
        content,
        at.line,
        col,
    )
}

fn assert_unsupported<T: std::fmt::Debug>(
    case: &str,
    got: Result<T, LspError>,
    want_method: &str,
    want_capability: &str,
) {
    match got {
        Err(LspError::Unsupported { method, capability }) => {
            assert_eq!(method, want_method, "{case}");
            assert_eq!(capability, want_capability, "{case}");
        }
        other => panic!(
            "{case}: want Unsupported({want_capability}) — if the server now advertises it, \
             move this row to the positive cases; got {other:?}"
        ),
    }
}

fn ends_with(loc: &SymbolLocation, file: &str) -> bool {
    Path::new(&loc.path).ends_with(file)
}

fn prepare_one(case: &str, c: &mut Client, root: &Path, at: At, want: &str) -> TypeTarget {
    let (path, content, line, col) = resolve(root, at);
    let targets = c
        .prepare_type_hierarchy(&path, &content, line, col)
        .unwrap_or_else(|e| panic!("{case}: prepare {want}: {e}"));
    targets
        .into_iter()
        .find(|t| t.name == want)
        .unwrap_or_else(|| panic!("{case}: prepare at {want} returned no {want} item"))
}

fn run_case(case: &Case) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root: PathBuf = dir.path().canonicalize().expect("canonical root");
    for (rel, content) in case.files {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        fs::write(&p, content).expect("fixture");
    }
    let argv: Vec<String> = case.server.iter().map(|s| s.to_string()).collect();
    let mut c = Client::new_stdio(&argv).unwrap_or_else(|e| panic!("{}: spawn: {e}", case.name));
    c.configure(ClientConfig {
        initialize_timeout: Duration::from_secs(60),
        call_timeout: Duration::from_secs(60),
        query_deadline: Duration::from_secs(60),
        ready_deadline: Duration::from_secs(120),
        ready_retry: Duration::from_millis(250),
        ..ClientConfig::default()
    });
    c.initialize(&lsp::file_uri(root.to_str().expect("utf-8")).to_string())
        .unwrap_or_else(|e| panic!("{}: initialize: {e}", case.name));

    if let Some(at) = case.probe {
        let (path, content, line, col) = resolve(&root, at);
        c.await_ready(&path, &content, line, col)
            .unwrap_or_else(|e| panic!("{}: never ready: {e}", case.name));
    }

    match &case.implementation {
        Some(Implementation::Finds { at, want_file }) => {
            let (path, content, line, col) = resolve(&root, *at);
            let locs = c
                .implementation(&path, &content, line, col)
                .unwrap_or_else(|e| panic!("{}: implementation: {e}", case.name));
            assert!(
                locs.iter().any(|l| ends_with(l, want_file)),
                "{}: implementation must land in {want_file}, got {locs:?}",
                case.name
            );
        }
        Some(Implementation::Unsupported { at }) => {
            let (path, content, line, col) = resolve(&root, *at);
            assert_unsupported(
                case.name,
                c.implementation(&path, &content, line, col),
                "textDocument/implementation",
                "implementationProvider",
            );
        }
        None => {}
    }

    match &case.type_hierarchy {
        Some(TypeHierarchy::Pair {
            sub,
            sub_name,
            sup,
            sup_name,
        }) => {
            let sub_t = prepare_one(case.name, &mut c, &root, *sub, sub_name);
            let supers = c
                .supertypes(&sub_t)
                .unwrap_or_else(|e| panic!("{}: supertypes: {e}", case.name));
            assert!(
                supers
                    .iter()
                    .any(|t| t.name == *sup_name && ends_with(&t.location, sup.file)),
                "{}: supertypes of {sub_name} must include {sup_name} in {}, got {supers:?}",
                case.name,
                sup.file
            );
            let sup_t = prepare_one(case.name, &mut c, &root, *sup, sup_name);
            let subs = c
                .subtypes(&sup_t)
                .unwrap_or_else(|e| panic!("{}: subtypes: {e}", case.name));
            assert!(
                subs.iter()
                    .any(|t| t.name == *sub_name && ends_with(&t.location, sub.file)),
                "{}: subtypes of {sup_name} must include {sub_name} in {}, got {subs:?}",
                case.name,
                sub.file
            );
        }
        Some(TypeHierarchy::Unsupported { at }) => {
            let (path, content, line, col) = resolve(&root, *at);
            assert_unsupported(
                case.name,
                c.prepare_type_hierarchy(&path, &content, line, col),
                "textDocument/prepareTypeHierarchy",
                "typeHierarchyProvider",
            );
        }
        None => {}
    }

    c.close()
        .unwrap_or_else(|e| panic!("{}: close: {e}", case.name));
}

#[test]
fn implementation_and_type_hierarchy_against_real_servers() {
    if std::env::var("BAGE_LSP_REAL_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP implementation_and_type_hierarchy_against_real_servers: set BAGE_LSP_REAL_TEST=1 to run"
        );
        return;
    }
    for case in CASES {
        eprintln!("real-server case: {}", case.name);
        run_case(case);
    }
}
