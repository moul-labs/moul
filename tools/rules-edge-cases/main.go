// Command rules-edge-cases asks the Go engine's rule package about cases its
// own test suite never tried.
//
// It reads cases, one JSON object per line, and writes the engine's answer to
// each, one per line in the same order. The answers come from a test that a
// build overlay adds to internal/rules of a checkout of
// github.com/moul-dev/moul-dev, run with `go test -overlay`. The checkout is
// never modified: the added and patched files live in a temporary directory.
// The overlay also replaces cloak.ComputeHash with "hash(<text>)", so SQL
// parameters of cloaked fields do not depend on a key.
//
// A case is one of:
//
//	{"rule", "auth", "record", "context"[, "setup"]}
//	    rules.EvaluateRule; with "setup", on a fresh in-memory modernc SQLite
//	    database built by those statements and then db.EnsureSystemTables
//	{"kind": "filter", "filter", "moul", "auth", "context", "param_prefix", "untrusted", "loader"}
//	    rules.BuildFilterSQLWithOptions; "loader" maps names to collections
//	{"kind": "sort", "filter", "moul", "untrusted", "loader"}
//	    rules.BuildSortSQLWithOptions
//
// Usage:
//
//	go run ./tools/rules-edge-cases -go-repo ../moul-dev/moul-dev -in cases.jsonl -out answers.jsonl
//
// scripts/export-rules-edge-cases.sh drives it; see crates/moul-rules/README.md.
package main

import (
	"bufio"
	"bytes"
	"embed"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

//go:embed overlay/*.tmpl
var templates embed.FS

const goModule = "github.com/moul-dev/moul-dev"

// patches rename functions of the Go repo that the overlay replaces.
var patches = []struct{ file, old, new string }{
	{"internal/cloak/cloak.go", "func ComputeHash(", "func moulecKeyedComputeHash("},
}

// added lists the new files the overlay puts into packages of the Go repo.
var added = []struct{ file, tmpl string }{
	{"internal/cloak/zz_moulec_hash.go", "overlay/hash.go.tmpl"},
	{"internal/rules/zz_moulec_record_test.go", "overlay/record_test.go.tmpl"},
}

func main() {
	goRepo := flag.String("go-repo", "", "path to a checkout of "+goModule+" (required)")
	in := flag.String("in", "", "cases, one JSON object per line (required)")
	out := flag.String("out", "", "where to write the answers (required)")
	allowDirty := flag.Bool("allow-dirty", false, "record from a checkout with uncommitted changes")
	flag.Parse()
	if *goRepo == "" || *in == "" || *out == "" || flag.NArg() > 0 {
		flag.Usage()
		os.Exit(2)
	}
	if err := run(*goRepo, *in, *out, *allowDirty); err != nil {
		fmt.Fprintln(os.Stderr, "rules-edge-cases:", err)
		os.Exit(1)
	}
}

func run(goRepo, in, out string, allowDirty bool) error {
	var err error
	for _, p := range []*string{&goRepo, &in, &out} {
		if *p, err = filepath.Abs(*p); err != nil {
			return err
		}
	}
	commit, err := runOut(goRepo, "git", "rev-parse", "HEAD")
	if err != nil {
		return fmt.Errorf("%s is not a git checkout: %w", goRepo, err)
	}
	if status, _ := runOut(goRepo, "git", "status", "--porcelain", "--untracked-files=no"); status != "" && !allowDirty {
		return fmt.Errorf("%s has uncommitted changes; commit them or pass -allow-dirty", goRepo)
	}
	cases, err := countLines(in)
	if err != nil {
		return err
	}

	work, err := os.MkdirTemp("", "moul-rules-edge-cases-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(work)
	overlay, err := writeOverlay(goRepo, work)
	if err != nil {
		return err
	}

	goVersion, _ := runOut(goRepo, "go", "env", "GOVERSION")
	fmt.Fprintf(os.Stderr, "answering %d cases with %s at %s (%s)\n", cases, goRepo, commit[:12], goVersion)
	answers := filepath.Join(work, "answers.jsonl")
	cmd := exec.Command("go", "test", "-count=1", "-run", "^TestMoulecRecord$", "-overlay", overlay, "./internal/rules")
	cmd.Dir = goRepo
	cmd.Env = append(os.Environ(), "MOULEC_IN="+in, "MOULEC_OUT="+answers)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("go test: %w", err)
	}
	got, err := countLines(answers)
	if err != nil {
		return fmt.Errorf("the recorder wrote no answers: %w", err)
	}
	if got != cases {
		return fmt.Errorf("%d cases but %d answers", cases, got)
	}
	b, err := os.ReadFile(answers)
	if err != nil {
		return err
	}
	return os.WriteFile(out, b, 0o644)
}

func runOut(dir, name string, args ...string) (string, error) {
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	cmd.Stderr = os.Stderr
	b, err := cmd.Output()
	return strings.TrimSpace(string(b)), err
}

func countLines(path string) (int, error) {
	f, err := os.Open(path)
	if err != nil {
		return 0, err
	}
	defer f.Close()
	n := 0
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 64<<20)
	for sc.Scan() {
		if len(bytes.TrimSpace(sc.Bytes())) > 0 {
			n++
		}
	}
	return n, sc.Err()
}

// writeOverlay writes the patched and added files and the overlay JSON that
// maps them onto the Go checkout, and returns the JSON's path.
func writeOverlay(goRepo, dir string) (string, error) {
	replace := map[string]string{}
	patched := map[string]string{}
	for _, p := range patches {
		src, ok := patched[p.file]
		if !ok {
			b, err := os.ReadFile(filepath.Join(goRepo, p.file))
			if err != nil {
				return "", err
			}
			src = string(b)
		}
		if n := strings.Count(src, p.old); n != 1 {
			return "", fmt.Errorf("%s: expected %q exactly once, found %d; the Go engine changed and tools/rules-edge-cases needs updating", p.file, p.old, n)
		}
		patched[p.file] = strings.Replace(src, p.old, p.new, 1)
	}
	for file, src := range patched {
		dst := filepath.Join(dir, strings.ReplaceAll(file, "/", "__"))
		if err := os.WriteFile(dst, []byte(src), 0o644); err != nil {
			return "", err
		}
		replace[filepath.Join(goRepo, file)] = dst
	}
	for _, a := range added {
		target := filepath.Join(goRepo, a.file)
		if _, err := os.Stat(target); err == nil {
			return "", fmt.Errorf("%s already exists in the Go repo", target)
		}
		b, err := templates.ReadFile(a.tmpl)
		if err != nil {
			return "", err
		}
		dst := filepath.Join(dir, strings.ReplaceAll(a.file, "/", "__"))
		if err := os.WriteFile(dst, b, 0o644); err != nil {
			return "", err
		}
		replace[target] = dst
	}
	doc, _ := json.MarshalIndent(map[string]any{"Replace": replace}, "", "  ")
	path := filepath.Join(dir, "overlay.json")
	return path, os.WriteFile(path, doc, 0o644)
}
