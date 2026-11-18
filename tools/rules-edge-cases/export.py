#!/usr/bin/env python3
"""Record the Go rule edge cases with the Go engine.

    python3 tools/rules-edge-cases/export.py --go-repo DIR --out DIR [--allow-dirty]

scripts/export-rules-edge-cases.sh runs it with the usual paths. The cases are
in cases.py; the answers come from the Go program in this directory, which runs
them through the engine's internal/rules (see main.go). Each case becomes one
line, {"input": case, "output": answer}; go_edge_cases_db.jsonl starts with
{"setup": [statements]}, which builds the database each of its cases runs on.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
import cases  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--go-repo", required=True, help="checkout of github.com/moul-dev/moul-dev")
    p.add_argument("--out", required=True, help="directory to write the .jsonl files to")
    p.add_argument("--allow-dirty", action="store_true", help="record from a checkout with uncommitted changes")
    args = p.parse_args()

    setup, db_cases = cases.evaluate_db()
    files = [
        ("go_edge_cases.jsonl", None, cases.evaluate()),
        ("go_edge_cases_db.jsonl", setup, db_cases),
        ("go_sql_edge_cases.jsonl", None, cases.sql()),
    ]

    with tempfile.TemporaryDirectory(prefix="moul-rules-edge-cases-") as work:
        src = os.path.join(work, "cases.jsonl")
        dst = os.path.join(work, "answers.jsonl")
        with open(src, "w", encoding="utf-8") as f:
            for _, setup, items in files:
                for c in items:
                    f.write(json.dumps(c if setup is None else dict(c, setup=setup)) + "\n")
        cmd = ["go", "run", ".", "-go-repo", args.go_repo, "-in", src, "-out", dst]
        if args.allow_dirty:
            cmd.append("-allow-dirty")
        if subprocess.run(cmd, cwd=HERE).returncode != 0:
            sys.exit(1)
        with open(dst, encoding="utf-8") as f:
            answers = [json.loads(line) for line in f if line.strip()]

    total = sum(len(items) for _, _, items in files)
    if len(answers) != total:
        sys.exit(f"export.py: {total} cases but {len(answers)} answers")
    os.makedirs(args.out, exist_ok=True)
    answers = iter(answers)
    for name, setup, items in files:
        lines = [] if setup is None else [json.dumps({"setup": setup})]
        lines += [json.dumps({"input": c, "output": next(answers)}) for c in items]
        path = os.path.join(args.out, name)
        with open(path, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")
        print(f"wrote {len(items)} cases to {path}", file=sys.stderr)
    note_commit(args.go_repo)


def note_commit(go_repo):
    """Says so when the Go checkout is not at the commit fixtures/go came from."""
    try:
        with open(os.path.join(ROOT, "fixtures", "go", "MANIFEST.json"), encoding="utf-8") as f:
            want = json.load(f)["commit"]
        got = subprocess.run(["git", "-C", go_repo, "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, KeyError, ValueError, subprocess.CalledProcessError):
        return
    if got != want:
        print(f"note: the Go checkout is at {got[:12]}, but fixtures/go/MANIFEST.json was exported at {want[:12]}", file=sys.stderr)


if __name__ == "__main__":
    main()
