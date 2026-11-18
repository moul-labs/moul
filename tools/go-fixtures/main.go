// Command go-fixtures records fixtures from the Go engine's own test suite.
//
// It runs `go test` in a checkout of github.com/moul-dev/moul-dev with a build
// overlay that wraps the HTTP router and the rule functions in recorders. The Go
// checkout is never modified: the patched files live in a temporary directory and
// reach the compiler through `go test -overlay`.
//
// Usage:
//
//	go run ./tools/go-fixtures -go-repo ../moul-dev/moul-dev
//
// See fixtures/go/README.md for the output format.
package main

import (
	"bufio"
	"bytes"
	"crypto/sha256"
	"embed"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
)

//go:embed overlay/*.tmpl
var templates embed.FS

const goModule = "github.com/moul-dev/moul-dev"

// patch describes one source file in the Go repo that the overlay replaces.
type patch struct {
	file string
	old  string
	new  string
}

var patches = []patch{
	{"internal/handlers/router.go", "\te := echo.New()\n", "\te := echo.New()\n\tmoulfxInstall(e, dbConn)\n"},
	{"internal/rules/evaluator.go", "func EvaluateRule(", "func moulfxOrigEvaluateRule("},
	{"internal/rules/sql_translator.go", "func BuildFilterSQLWithOptions(", "func moulfxOrigBuildFilterSQLWithOptions("},
	{"internal/rules/sql_translator.go", "func BuildSortSQLWithOptions(", "func moulfxOrigBuildSortSQLWithOptions("},
}

// added lists the new files the overlay puts into packages of the Go repo.
var added = []struct{ dir, name, tmpl, pkg string }{
	{"internal/handlers", "zz_moulfx_common.go", "overlay/common.go.tmpl", "handlers"},
	{"internal/handlers", "zz_moulfx_http.go", "overlay/http.go.tmpl", ""},
	{"internal/rules", "zz_moulfx_common.go", "overlay/common.go.tmpl", "rules"},
	{"internal/rules", "zz_moulfx_rules.go", "overlay/rules.go.tmpl", ""},
}

func main() {
	goRepo := flag.String("go-repo", "", "path to a checkout of "+goModule+" (required)")
	out := flag.String("out", "fixtures/go", "output directory, replaced on every run")
	pkgs := flag.String("pkgs", "./...", "packages to test, space separated")
	timeout := flag.String("timeout", "30m", "go test -timeout")
	allowDirty := flag.Bool("allow-dirty", false, "export from a checkout with uncommitted changes")
	keepRaw := flag.Bool("keep-raw", false, "keep the raw recordings and print where they are")
	flag.Parse()
	if *goRepo == "" {
		flag.Usage()
		os.Exit(2)
	}
	if err := run(*goRepo, *out, strings.Fields(*pkgs), *timeout, *allowDirty, *keepRaw); err != nil {
		fmt.Fprintln(os.Stderr, "go-fixtures:", err)
		os.Exit(1)
	}
}

func run(goRepo, out string, pkgs []string, timeout string, allowDirty, keepRaw bool) error {
	goRepo, err := filepath.Abs(goRepo)
	if err != nil {
		return err
	}
	out, err = filepath.Abs(out)
	if err != nil {
		return err
	}

	commit, err := git(goRepo, "rev-parse", "HEAD")
	if err != nil {
		return fmt.Errorf("%s is not a git checkout: %w", goRepo, err)
	}
	status, _ := git(goRepo, "status", "--porcelain", "--untracked-files=no")
	dirty := status != ""
	if dirty && !allowDirty {
		return fmt.Errorf("%s has uncommitted changes; commit them or pass -allow-dirty", goRepo)
	}
	remote, _ := git(goRepo, "remote", "get-url", "origin")

	work, err := os.MkdirTemp("", "moul-go-fixtures-")
	if err != nil {
		return err
	}
	if keepRaw {
		fmt.Fprintln(os.Stderr, "raw recordings kept in", work)
	} else {
		defer os.RemoveAll(work)
	}
	raw := filepath.Join(work, "raw")
	if err := os.MkdirAll(raw, 0o755); err != nil {
		return err
	}

	overlayPath, err := writeOverlay(goRepo, filepath.Join(work, "overlay"))
	if err != nil {
		return err
	}

	goVersion, _ := runOut(goRepo, nil, "go", "env", "GOVERSION")
	fmt.Fprintf(os.Stderr, "running go test in %s at %s (%s)\n", goRepo, commit[:12], goVersion)
	started := time.Now()
	testLog := filepath.Join(work, "test2json.jsonl")
	if err := goTest(goRepo, raw, overlayPath, testLog, timeout, pkgs); err != nil {
		var exit *exec.ExitError
		if !errors.As(err, &exit) {
			return err
		}
		fmt.Fprintln(os.Stderr, "go test reported failures; fixtures from failing tests are marked outcome=fail")
	}
	fmt.Fprintf(os.Stderr, "go test finished in %s\n", time.Since(started).Round(time.Second))

	outcomes, err := readOutcomes(testLog)
	if err != nil {
		return err
	}
	return collate(raw, out, outcomes, manifest{
		GoRepo:     remote,
		Commit:     commit,
		Dirty:      dirty,
		GoVersion:  goVersion,
		Packages:   pkgs,
		ExportedAt: time.Now().UTC().Format(time.RFC3339),
	})
}

func git(dir string, args ...string) (string, error) {
	return runOut(dir, nil, "git", args...)
}

func runOut(dir string, env []string, name string, args ...string) (string, error) {
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), env...)
	cmd.Stderr = os.Stderr
	b, err := cmd.Output()
	return strings.TrimSpace(string(b)), err
}

// writeOverlay writes the patched and added files and the overlay JSON that maps
// them onto the Go checkout, and returns the JSON's path.
func writeOverlay(goRepo, dir string) (string, error) {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
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
			return "", fmt.Errorf("%s: expected %q exactly once, found %d; the Go engine changed and tools/go-fixtures needs updating", p.file, p.old, n)
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
		b, err := templates.ReadFile(a.tmpl)
		if err != nil {
			return "", err
		}
		if a.pkg != "" {
			b = bytes.Replace(b, []byte("package PACKAGE"), []byte("package "+a.pkg), 1)
		}
		target := filepath.Join(goRepo, a.dir, a.name)
		if _, err := os.Stat(target); err == nil {
			return "", fmt.Errorf("%s already exists in the Go repo", target)
		}
		dst := filepath.Join(dir, strings.ReplaceAll(a.dir, "/", "__")+"__"+a.name)
		if err := os.WriteFile(dst, b, 0o644); err != nil {
			return "", err
		}
		replace[target] = dst
	}
	doc, _ := json.MarshalIndent(map[string]any{"Replace": replace}, "", "  ")
	path := filepath.Join(dir, "overlay.json")
	return path, os.WriteFile(path, doc, 0o644)
}

func goTest(goRepo, raw, overlay, log, timeout string, pkgs []string) error {
	f, err := os.Create(log)
	if err != nil {
		return err
	}
	defer f.Close()
	args := append([]string{"test", "-json", "-count=1", "-timeout", timeout, "-overlay", overlay}, pkgs...)
	cmd := exec.Command("go", args...)
	cmd.Dir = goRepo
	cmd.Env = append(os.Environ(), "MOUL_FIXTURE_RAW="+raw)
	cmd.Stdout = f
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

// outcomes maps "importpath TestName" to pass, fail or skip.
type outcomes map[string]string

func readOutcomes(path string) (outcomes, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	res := outcomes{}
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 64<<20)
	for sc.Scan() {
		var ev struct{ Action, Package, Test string }
		if json.Unmarshal(sc.Bytes(), &ev) != nil || ev.Test == "" || strings.Contains(ev.Test, "/") {
			continue
		}
		switch ev.Action {
		case "pass", "fail", "skip":
			res[ev.Package+" "+ev.Test] = ev.Action
		}
	}
	return res, sc.Err()
}

type manifest struct {
	GoRepo     string         `json:"go_repo"`
	Commit     string         `json:"commit"`
	Dirty      bool           `json:"dirty"`
	GoVersion  string         `json:"go_version"`
	Packages   []string       `json:"packages"`
	ExportedAt string         `json:"exported_at"`
	Counts     map[string]int `json:"counts"`
	FailedTest []string       `json:"failed_tests"`
	Unmapped   []string       `json:"unmapped_binaries,omitempty"`
}

type rawLine struct {
	Kind     string          `json:"kind"`
	Seq      int64           `json:"seq"`
	Binary   string          `json:"binary"`
	Session  int64           `json:"session"`
	Test     string          `json:"test"`
	DB       string          `json:"db"`
	DBError  string          `json:"db_error"`
	Request  json.RawMessage `json:"request"`
	Response json.RawMessage `json:"response"`
	Route    string          `json:"route"`
	Input    json.RawMessage `json:"input"`
	Output   json.RawMessage `json:"output"`
}

type exchange struct {
	Route    string          `json:"route"`
	Request  json.RawMessage `json:"request"`
	Response json.RawMessage `json:"response"`
}

type session struct {
	id        int64
	Test      string     `json:"-"`
	DBBefore  string     `json:"db_before,omitempty"`
	DBError   string     `json:"db_error,omitempty"`
	Exchanges []exchange `json:"exchanges"`
}

type ruleCase struct {
	ID      string          `json:"id"`
	Sources []string        `json:"sources"`
	Input   json.RawMessage `json:"input"`
	Output  json.RawMessage `json:"output"`
}

var dbRef = regexp.MustCompile(`"db(?:_before)?":\s*"([0-9a-f]{16})"`)

func collate(raw, out string, outcomes outcomes, m manifest) error {
	files, err := filepath.Glob(filepath.Join(raw, "raw-*.jsonl"))
	if err != nil {
		return err
	}
	sort.Strings(files)

	// The test binary is named after the package's last path element; map it back.
	pkgOf := map[string][]string{}
	for key := range outcomes {
		pkg := strings.Fields(key)[0]
		bin := pkg[strings.LastIndex(pkg, "/")+1:] + ".test"
		if !contains(pkgOf[bin], pkg) {
			pkgOf[bin] = append(pkgOf[bin], pkg)
		}
	}
	unmapped := map[string]bool{}
	pkgFor := func(bin string) string {
		if p := pkgOf[bin]; len(p) == 1 {
			return p[0]
		}
		unmapped[bin] = true
		return "unknown/" + strings.TrimSuffix(bin, ".test")
	}

	type testKey struct{ pkg, test string }
	httpByTest := map[testKey][]*session{}
	rules := map[string]map[string]*ruleCase{} // kind -> id -> case

	for _, path := range files {
		sessions := map[int64]*session{}
		var order []int64
		var pkg string
		err := eachLine(path, func(b []byte) error {
			var l rawLine
			if err := json.Unmarshal(b, &l); err != nil {
				return fmt.Errorf("%s: %w", path, err)
			}
			if pkg == "" {
				pkg = pkgFor(l.Binary)
			}
			get := func() *session {
				s := sessions[l.Session]
				if s == nil {
					s = &session{id: l.Session}
					sessions[l.Session] = s
					order = append(order, l.Session)
				}
				return s
			}
			switch l.Kind {
			case "http_session":
				get().Test = l.Test
			case "http_db_before":
				s := get()
				s.DBBefore, s.DBError = l.DB, l.DBError
			case "http_exchange":
				s := get()
				s.Exchanges = append(s.Exchanges, exchange{Route: l.Route, Request: l.Request, Response: l.Response})
			case "rule_evaluate", "rule_filter_sql", "rule_sort_sql":
				kind := strings.TrimPrefix(l.Kind, "rule_")
				sum := sha256.Sum256(append(append([]byte{}, l.Input...), l.Output...))
				id := hex.EncodeToString(sum[:])[:16]
				if rules[kind] == nil {
					rules[kind] = map[string]*ruleCase{}
				}
				c := rules[kind][id]
				if c == nil {
					c = &ruleCase{ID: id, Input: l.Input, Output: l.Output}
					rules[kind][id] = c
				}
				rel := strings.TrimPrefix(pkg, goModule+"/")
				src := rel + "." + l.Test
				if l.Test == "" {
					src = rel + ".(server)"
				}
				if !contains(c.Sources, src) {
					c.Sources = append(c.Sources, src)
				}
			default:
				return fmt.Errorf("%s: unknown record kind %q", path, l.Kind)
			}
			return nil
		})
		if err != nil {
			return err
		}
		for _, id := range order {
			s := sessions[id]
			if len(s.Exchanges) == 0 {
				continue
			}
			test := s.Test
			if test == "" {
				test = "_unattributed"
			}
			k := testKey{pkg, test}
			httpByTest[k] = append(httpByTest[k], s)
		}
	}

	for _, sub := range []string{"http", "rules", "db"} {
		if err := os.RemoveAll(filepath.Join(out, sub)); err != nil {
			return err
		}
	}
	usedDB := map[string]bool{}
	counts := map[string]int{}
	var failed []string

	for k, sessions := range httpByTest {
		outcome := outcomes[k.pkg+" "+k.test]
		if outcome == "" {
			outcome = "unknown"
		}
		if outcome == "fail" {
			failed = append(failed, k.pkg+"."+k.test)
		}
		doc := map[string]any{
			"package":  strings.TrimPrefix(k.pkg, goModule+"/"),
			"test":     k.test,
			"outcome":  outcome,
			"sessions": sessions,
		}
		b, err := json.MarshalIndent(doc, "", "  ")
		if err != nil {
			return err
		}
		markDB(b, usedDB)
		dir := filepath.Join(out, "http", strings.TrimPrefix(k.pkg, goModule+"/"))
		if err := writeFile(filepath.Join(dir, k.test+".json"), b); err != nil {
			return err
		}
		counts["http_tests"]++
		counts["http_sessions"] += len(sessions)
		for _, s := range sessions {
			counts["http_exchanges"] += len(s.Exchanges)
		}
	}

	for kind, cases := range rules {
		ids := make([]string, 0, len(cases))
		for id := range cases {
			ids = append(ids, id)
		}
		sort.Strings(ids)
		var buf bytes.Buffer
		for _, id := range ids {
			c := cases[id]
			sort.Strings(c.Sources)
			b, err := json.Marshal(c)
			if err != nil {
				return err
			}
			markDB(b, usedDB)
			buf.Write(b)
			buf.WriteByte('\n')
		}
		if err := writeFile(filepath.Join(out, "rules", kind+".jsonl"), buf.Bytes()); err != nil {
			return err
		}
		counts["rules_"+kind] = len(ids)
	}

	for id := range usedDB {
		b, err := os.ReadFile(filepath.Join(raw, "db", id+".json"))
		if err != nil {
			return fmt.Errorf("database dump %s: %w", id, err)
		}
		if err := writeFile(filepath.Join(out, "db", id+".json"), b); err != nil {
			return err
		}
	}
	counts["db_states"] = len(usedDB)
	for _, o := range outcomes {
		if o != "skip" {
			counts["go_tests_run"]++
		}
	}

	sort.Strings(failed)
	m.Counts = counts
	m.FailedTest = failed
	for bin := range unmapped {
		m.Unmapped = append(m.Unmapped, bin)
	}
	sort.Strings(m.Unmapped)
	b, _ := json.MarshalIndent(m, "", "  ")
	if err := writeFile(filepath.Join(out, "MANIFEST.json"), append(b, '\n')); err != nil {
		return err
	}
	fmt.Fprintf(os.Stderr, "wrote %s: %v\n", out, counts)
	if len(failed) > 0 {
		fmt.Fprintf(os.Stderr, "%d tests with fixtures failed in Go; see MANIFEST.json\n", len(failed))
	}
	return nil
}

func markDB(b []byte, used map[string]bool) {
	for _, m := range dbRef.FindAllSubmatch(b, -1) {
		used[string(m[1])] = true
	}
}

func eachLine(path string, fn func([]byte) error) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	r := bufio.NewReaderSize(f, 1<<20)
	for {
		line, err := r.ReadBytes('\n')
		if len(bytes.TrimSpace(line)) > 0 {
			if ferr := fn(line); ferr != nil {
				return ferr
			}
		}
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return err
		}
	}
}

func writeFile(path string, b []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	if len(b) > 0 && b[len(b)-1] != '\n' {
		b = append(b, '\n')
	}
	return os.WriteFile(path, b, 0o644)
}

func contains(xs []string, x string) bool {
	for _, y := range xs {
		if y == x {
			return true
		}
	}
	return false
}
