package bage_test

import (
	"context"
	"fmt"
	"os"
	"path/filepath"

	"github.com/hylla-io/bage/internal/hashing"
	"github.com/hylla-io/bage/internal/region"
	"github.com/hylla-io/bage/pkg/bage"
)

// ExampleEditor_Apply opens an Editor over a temp WALDir with the default
// XXHasher and no formatter, then applies a single region-anchored edit to a
// temp Go file. The edit targets the byte range covering "hi" and carries the
// matching region_hash so Resolve verifies the content before splicing. It is
// fully hermetic: no language server and no container are involved.
func ExampleEditor_Apply() {
	dir, err := os.MkdirTemp("", "bage-example-*")
	if err != nil {
		panic(err)
	}
	defer os.RemoveAll(dir)

	src := "package main\n\nvar Greeting = \"hi\"\n"
	file := filepath.Join(dir, "greeting.go")
	if err := os.WriteFile(file, []byte(src), 0o644); err != nil {
		panic(err)
	}

	ed, err := bage.Open(bage.Config{
		Lang:   bage.LangGo,
		WALDir: dir,
	})
	if err != nil {
		panic(err)
	}
	defer ed.Close()

	// Anchor the edit to the byte range covering "hi" with its region_hash, so
	// the resolver verifies the targeted content before applying "hello".
	start := len("package main\n\nvar Greeting = \"")
	end := start + len("hi")
	live := []byte(src)
	hasher := hashing.XXHasher{}

	edit := bage.Edit{
		Region: bage.Region{
			Path:       file,
			StartByte:  start,
			EndByte:    end,
			RegionHash: region.HashRegion(live, start, end),
		},
		NewText: "hello",
	}
	anchor := bage.FileAnchor{
		Path:     file,
		RawHash:  hashing.RawHash(hasher, live),
		NormHash: hashing.NormHash(hasher, live),
	}

	results, err := ed.Apply(context.Background(), []bage.Edit{edit}, []bage.FileAnchor{anchor})
	if err != nil {
		panic(err)
	}

	out, err := os.ReadFile(file)
	if err != nil {
		panic(err)
	}
	fmt.Printf("changed [%d:%d] new lines %d-%d\n", results[0].ChangedStart, results[0].ChangedEnd, results[0].NewStartLine, results[0].NewEndLine)
	fmt.Print(string(out))
	// Output:
	// changed [30:35] new lines 3-3
	// package main
	//
	// var Greeting = "hello"
}
