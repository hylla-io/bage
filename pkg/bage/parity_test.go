package bage_test

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"

	"github.com/hylla-io/bage/pkg/bage"
)

// parityCase is one row of the polyglot file-lifecycle parity matrix: a real,
// idiomatic file of one language/type the facade must be able to CREATE, PARSE
// (parse-floor clean), and round-trip (recomputed raw/norm hashes match the
// create result) entirely through the PUBLIC pkg/bage facade — proving a host
// like Hylla never needs internal/*.
type parityCase struct {
	// name is the subtest label and the language/type under test.
	name string
	// file is the basename whose extension/name drives auto-detect (LangForPath).
	file string
	// wantLang is the language LangForPath must resolve for file.
	wantLang bage.Lang
	// content is REAL, idiomatic representative content of this type.
	content string
}

// parityMatrix is the full language/file-type matrix bage supports, each with
// REAL idiomatic content (not 'x = 1' everywhere). It spans every registered
// grammar (Go, Python, TS/TSX, JS/JSX, Rust, Java, C, C++, C#, Ruby, JSON, HTML,
// CSS, YAML, TOML, XML, Makefile, Bash, Markdown) AND the grammar-free
// text-fallback types (MDX, SCSS, Dockerfile, .txt, a dotfile) — every one must
// create, parse clean, and round-trip through the facade.
func parityMatrix() []parityCase {
	return []parityCase{
		{
			name:     "Go",
			file:     "server.go",
			wantLang: bage.LangGo,
			content:  "package server\n\nimport \"fmt\"\n\n// Greet returns a greeting for name.\nfunc Greet(name string) string {\n\treturn fmt.Sprintf(\"hello, %s\", name)\n}\n",
		},
		{
			name:     "Python",
			file:     "app.py",
			wantLang: bage.LangPython,
			content:  "import sys\n\n\ndef greet(name: str) -> str:\n    \"\"\"Return a greeting for name.\"\"\"\n    return f\"hello, {name}\"\n\n\nif __name__ == \"__main__\":\n    print(greet(sys.argv[1]))\n",
		},
		{
			name:     "TypeScript",
			file:     "greet.ts",
			wantLang: bage.LangTypeScript,
			content:  "export interface User {\n  name: string;\n  age: number;\n}\n\nexport function greet(user: User): string {\n  return `hello, ${user.name}`;\n}\n",
		},
		{
			name:     "TSX",
			file:     "Button.tsx",
			wantLang: bage.LangTSX,
			content:  "import React from \"react\";\n\ninterface Props {\n  label: string;\n}\n\nexport const Button = ({ label }: Props): JSX.Element => {\n  return <button className=\"btn\">{label}</button>;\n};\n",
		},
		{
			name:     "JavaScript",
			file:     "util.js",
			wantLang: bage.LangJavaScript,
			content:  "const PI = 3.14159;\n\nexport function area(radius) {\n  return PI * radius * radius;\n}\n\nexport default { area };\n",
		},
		{
			name:     "JSX",
			file:     "Card.jsx",
			wantLang: bage.LangJavaScript,
			content:  "import React from \"react\";\n\nexport function Card({ title }) {\n  return (\n    <div className=\"card\">\n      <h2>{title}</h2>\n    </div>\n  );\n}\n",
		},
		{
			name:     "Rust",
			file:     "lib.rs",
			wantLang: bage.LangRust,
			content:  "pub struct Point {\n    pub x: f64,\n    pub y: f64,\n}\n\nimpl Point {\n    pub fn norm(&self) -> f64 {\n        (self.x * self.x + self.y * self.y).sqrt()\n    }\n}\n",
		},
		{
			name:     "Java",
			file:     "Greeter.java",
			wantLang: bage.LangJava,
			content:  "package com.example;\n\npublic final class Greeter {\n    public String greet(String name) {\n        return \"hello, \" + name;\n    }\n}\n",
		},
		{
			name:     "C",
			file:     "hello.c",
			wantLang: bage.LangC,
			content:  "#include <stdio.h>\n\nint main(void) {\n    printf(\"hello, world\\n\");\n    return 0;\n}\n",
		},
		{
			name:     "C++",
			file:     "vec.cpp",
			wantLang: bage.LangCPP,
			content:  "#include <vector>\n#include <numeric>\n\nint sum(const std::vector<int>& xs) {\n    return std::accumulate(xs.begin(), xs.end(), 0);\n}\n",
		},
		{
			name:     "C#",
			file:     "Greeter.cs",
			wantLang: bage.LangCSharp,
			content:  "namespace Example;\n\npublic sealed class Greeter\n{\n    public string Greet(string name) => $\"hello, {name}\";\n}\n",
		},
		{
			name:     "Ruby",
			file:     "greeter.rb",
			wantLang: bage.LangRuby,
			content:  "# frozen_string_literal: true\n\nclass Greeter\n  def greet(name)\n    \"hello, #{name}\"\n  end\nend\n",
		},
		{
			name:     "JSON",
			file:     "package.json",
			wantLang: bage.LangJSON,
			content:  "{\n  \"name\": \"bage\",\n  \"version\": \"0.2.0\",\n  \"private\": true,\n  \"scripts\": {\n    \"build\": \"tsc\"\n  }\n}\n",
		},
		{
			name:     "HTML",
			file:     "index.html",
			wantLang: bage.LangHTML,
			content:  "<!DOCTYPE html>\n<html lang=\"en\">\n  <head>\n    <meta charset=\"utf-8\" />\n    <title>Båge</title>\n  </head>\n  <body>\n    <h1>Hello</h1>\n  </body>\n</html>\n",
		},
		{
			name:     "CSS",
			file:     "style.css",
			wantLang: bage.LangCSS,
			content:  ".btn {\n  display: inline-flex;\n  padding: 0.5rem 1rem;\n  color: #fff;\n  background: #2563eb;\n}\n",
		},
		{
			name:     "YAML",
			file:     "ci.yaml",
			wantLang: bage.LangYAML,
			content:  "name: ci\non:\n  push:\n    branches: [main]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
		},
		{
			name:     "TOML",
			file:     "Cargo.toml",
			wantLang: bage.LangTOML,
			content:  "[package]\nname = \"bage\"\nversion = \"0.2.0\"\nedition = \"2021\"\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n",
		},
		{
			name:     "XML",
			file:     "pom.xml",
			wantLang: bage.LangXML,
			content:  "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project>\n  <modelVersion>4.0.0</modelVersion>\n  <groupId>io.hylla</groupId>\n  <artifactId>bage</artifactId>\n</project>\n",
		},
		{
			name:     "Makefile",
			file:     "Makefile",
			wantLang: bage.LangMakefile,
			content:  "BINARY := bage\n\n.PHONY: build\nbuild:\n\tgo build -o bin/$(BINARY) ./cmd/bage\n\nclean:\n\trm -rf bin\n",
		},
		{
			name:     "Bash",
			file:     "deploy.sh",
			wantLang: bage.LangBash,
			content:  "#!/usr/bin/env bash\nset -euo pipefail\n\nmain() {\n  echo \"deploying ${1:-staging}\"\n}\n\nmain \"$@\"\n",
		},
		{
			name:     "Markdown",
			file:     "README.md",
			wantLang: bage.LangMarkdown,
			content:  "# Båge\n\nA region-anchored editor.\n\n## Usage\n\n```go\ned, _ := bage.Open(cfg)\n```\n\n- fast\n- safe\n",
		},
		// --- grammar-free text-fallback types (LangText): no registered grammar,
		// must still create, parse clean (lossless), and round-trip. ---
		{
			name:     "MDX",
			file:     "post.mdx",
			wantLang: bage.LangText,
			content:  "import { Note } from \"./Note\";\n\n# Title\n\n<Note>Hello from MDX</Note>\n",
		},
		{
			name:     "SCSS",
			file:     "theme.scss",
			wantLang: bage.LangText,
			content:  "$primary: #2563eb;\n\n.btn {\n  background: $primary;\n  &:hover {\n    background: darken($primary, 10%);\n  }\n}\n",
		},
		{
			name:     "Dockerfile",
			file:     "Dockerfile",
			wantLang: bage.LangText,
			content:  "FROM golang:1.22 AS build\nWORKDIR /src\nCOPY . .\nRUN go build -o /bin/bage ./cmd/bage\n\nFROM gcr.io/distroless/base\nCOPY --from=build /bin/bage /bin/bage\nENTRYPOINT [\"/bin/bage\"]\n",
		},
		{
			name:     "PlainText",
			file:     "notes.txt",
			wantLang: bage.LangText,
			content:  "Release checklist\n-----------------\n1. run mage ci\n2. tag the version\n3. push the tag\n",
		},
		{
			name:     "Dotfile",
			file:     ".env",
			wantLang: bage.LangText,
			content:  "DATABASE_URL=postgres://localhost:5432/bage\nLOG_LEVEL=info\nPORT=8080\n",
		},
	}
}

// TestParityCreateAllLangs is the polyglot parity floor: for EVERY language/file
// type in the matrix it CREATES a real file with REAL idiomatic content through
// the PUBLIC facade (Editor.Create) and asserts the file (1) lands on disk with
// the exact bytes, (2) auto-detects to the expected language, (3) PARSES clean
// (ParseHealth reports no defect) via the same public OpenFile/ParseHealth a host
// uses, and (4) ROUND-TRIPS — the raw/norm/region hashes recomputed from the
// on-disk bytes match the EditResult the facade returned. A host drives ALL of
// this through pkg/bage WITHOUT importing internal/*.
func TestParityCreateAllLangs(t *testing.T) {
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	ctx := context.Background()
	h := bage.XXHasher{}

	for _, tc := range parityMatrix() {
		tc := tc
		t.Run(tc.name, func(t *testing.T) {
			// Auto-detect must resolve the expected grammar for this type.
			if got := bage.LangForPath(tc.file); got != tc.wantLang {
				t.Fatalf("LangForPath(%q) = %v, want %v", tc.file, got, tc.wantLang)
			}

			path := filepath.Join(t.TempDir(), tc.file)
			res, err := ed.Create(ctx, bage.Op{Kind: bage.OpCreate, Path: path, Content: tc.content})
			if err != nil {
				t.Fatalf("Create(%s): %v", tc.name, err)
			}

			// (1) the bytes landed exactly.
			onDisk, err := os.ReadFile(path)
			if err != nil {
				t.Fatalf("read created %q: %v", path, err)
			}
			if string(onDisk) != tc.content {
				t.Fatalf("on-disk content mismatch\n got: %q\nwant: %q", onDisk, tc.content)
			}

			// (2) it opens to the expected language and (3) parses clean.
			opened, err := bage.OpenFile(ctx, path)
			if err != nil {
				t.Fatalf("OpenFile(%s): %v", tc.name, err)
			}
			defer opened.Close()
			if opened.Lang != tc.wantLang {
				t.Errorf("opened lang = %v, want %v", opened.Lang, tc.wantLang)
			}
			if defects := bage.ParseHealth(opened); len(defects) != 0 {
				t.Fatalf("parse-floor defects for %s: %+v", tc.name, defects)
			}

			// (4) round-trip: hashes recomputed from the live bytes match the result.
			if want := bage.RawHash(h, onDisk); res.NewFileRawHash != want {
				t.Errorf("raw hash: result %q, recompute %q", res.NewFileRawHash, want)
			}
			if want := bage.NormHash(h, onDisk); res.NewFileNormHash != want {
				t.Errorf("norm hash: result %q, recompute %q", res.NewFileNormHash, want)
			}
			if want := bage.RegionHash(onDisk, 0, len(onDisk)); res.NewRegionHash != want {
				t.Errorf("region hash: result %q, recompute %q", res.NewRegionHash, want)
			}
			if res.Path != path {
				t.Errorf("result path = %q, want %q", res.Path, path)
			}
		})
	}
}

// TestParityMixedLangApplyBatch drives a HETEROGENEOUS, mixed-language create
// batch (a .go + .py + .md + .json) through the PUBLIC facade as ONE
// all-or-nothing ApplyBatch. It proves the batch lands every file with the right
// auto-detected grammar and that each result round-trips — the exact shape a host
// maps to one graph mutation, with no internal/* import.
func TestParityMixedLangApplyBatch(t *testing.T) {
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	ctx := context.Background()
	h := bage.XXHasher{}
	dir := t.TempDir()

	files := []struct {
		path     string
		content  string
		wantLang bage.Lang
	}{
		{filepath.Join(dir, "main.go"), "package main\n\nfunc main() {}\n", bage.LangGo},
		{filepath.Join(dir, "app.py"), "def main() -> None:\n    pass\n", bage.LangPython},
		{filepath.Join(dir, "README.md"), "# Title\n\nbody text\n", bage.LangMarkdown},
		{filepath.Join(dir, "data.json"), "{\n  \"ok\": true\n}\n", bage.LangJSON},
	}

	ops := make([]bage.Op, len(files))
	for i, f := range files {
		ops[i] = bage.Op{Kind: bage.OpCreate, Path: f.path, Content: f.content}
	}

	results, err := ed.ApplyBatch(ctx, ops)
	if err != nil {
		t.Fatalf("ApplyBatch: %v", err)
	}
	if len(results) != len(files) {
		t.Fatalf("got %d results, want %d", len(results), len(files))
	}

	for i, f := range files {
		onDisk, err := os.ReadFile(f.path)
		if err != nil {
			t.Fatalf("read %q: %v", f.path, err)
		}
		if string(onDisk) != f.content {
			t.Errorf("%s content = %q, want %q", f.path, onDisk, f.content)
		}
		res := results[i]
		if res.Kind != bage.OpCreate {
			t.Errorf("result[%d] kind = %v, want OpCreate", i, res.Kind)
		}
		if want := bage.RawHash(h, onDisk); res.Create.NewFileRawHash != want {
			t.Errorf("%s raw hash: result %q, recompute %q", f.path, res.Create.NewFileRawHash, want)
		}

		opened, err := bage.OpenFile(ctx, f.path)
		if err != nil {
			t.Fatalf("OpenFile %q: %v", f.path, err)
		}
		if opened.Lang != f.wantLang {
			t.Errorf("%s lang = %v, want %v", f.path, opened.Lang, f.wantLang)
		}
		if defects := bage.ParseHealth(opened); len(defects) != 0 {
			t.Errorf("%s parse defects: %+v", f.path, defects)
		}
		opened.Close()
	}
}

// TestParityCreateDeleteMoveRoundTrip exercises the full single-op lifecycle
// through the PUBLIC facade: Create a file, Move it (anchored by its create-time
// raw_hash), then Delete the destination (anchored by the relocated raw_hash).
// It proves the source vanishes on move, the destination holds the unchanged
// bytes, and the delete clears the destination — the create→move→delete spine a
// host drives without touching internal/*.
func TestParityCreateDeleteMoveRoundTrip(t *testing.T) {
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	ctx := context.Background()
	dir := t.TempDir()
	src := filepath.Join(dir, "orig.go")
	dst := filepath.Join(dir, "moved.go")
	content := "package main\n\n// Run is the entrypoint.\nfunc Run() {}\n"

	// Create.
	createRes, err := ed.Create(ctx, bage.Op{Kind: bage.OpCreate, Path: src, Content: content})
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	// Move, anchored on the create-time raw_hash (no clobber, source must match).
	moveRes, err := ed.Move(ctx, bage.Op{
		Kind:            bage.OpMove,
		Path:            src,
		To:              dst,
		ExpectedRawHash: createRes.NewFileRawHash,
	})
	if err != nil {
		t.Fatalf("Move: %v", err)
	}
	if moveRes.From != src {
		t.Errorf("move From = %q, want %q", moveRes.From, src)
	}
	if _, err := os.Stat(src); !os.IsNotExist(err) {
		t.Errorf("source still exists after move (err=%v)", err)
	}
	moved, err := os.ReadFile(dst)
	if err != nil {
		t.Fatalf("read moved %q: %v", dst, err)
	}
	if string(moved) != content {
		t.Errorf("moved content = %q, want %q (relocate must preserve bytes)", moved, content)
	}

	// Delete the destination, anchored on the relocated raw_hash.
	delRes, err := ed.Delete(ctx, bage.Op{
		Kind:            bage.OpDelete,
		Path:            dst,
		ExpectedRawHash: moveRes.Dest.NewFileRawHash,
	})
	if err != nil {
		t.Fatalf("Delete: %v", err)
	}
	if delRes.Path != dst {
		t.Errorf("delete Path = %q, want %q", delRes.Path, dst)
	}
	if _, err := os.Stat(dst); !os.IsNotExist(err) {
		t.Errorf("destination still exists after delete (err=%v)", err)
	}
}

// TestParityLifecycleAnchorRejects proves the public facade preserves the
// engine's safety anchors end-to-end: a Create over an existing path rejects with
// ErrExists (no clobber), a Delete with a stale raw_hash rejects as a
// *ConflictError (matchable via ErrConflict), and a Delete of a missing path
// rejects with ErrNotFound — all surfaced through the re-exported sentinels so a
// host never imports internal/*.
func TestParityLifecycleAnchorRejects(t *testing.T) {
	ed, err := bage.Open(bage.Config{WALDir: t.TempDir()})
	if err != nil {
		t.Fatalf("Open: %v", err)
	}
	defer ed.Close()

	ctx := context.Background()
	dir := t.TempDir()
	path := filepath.Join(dir, "x.go")
	content := "package x\n"

	res, err := ed.Create(ctx, bage.Op{Kind: bage.OpCreate, Path: path, Content: content})
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	// Clobber reject.
	if _, err := ed.Create(ctx, bage.Op{Kind: bage.OpCreate, Path: path, Content: "package y\n"}); !errors.Is(err, bage.ErrExists) {
		t.Errorf("clobber Create err = %v, want ErrExists", err)
	}

	// Drift reject (wrong raw_hash) → ConflictError.
	if _, err := ed.Delete(ctx, bage.Op{Kind: bage.OpDelete, Path: path, ExpectedRawHash: "deadbeefdeadbeef"}); !errors.Is(err, bage.ErrConflict) {
		t.Errorf("drift Delete err = %v, want ErrConflict", err)
	}

	// Missing-path reject.
	missing := filepath.Join(dir, "nope.go")
	if _, err := ed.Delete(ctx, bage.Op{Kind: bage.OpDelete, Path: missing, ExpectedRawHash: res.NewFileRawHash}); !errors.Is(err, bage.ErrNotFound) {
		t.Errorf("missing Delete err = %v, want ErrNotFound", err)
	}
}
