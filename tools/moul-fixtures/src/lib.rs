//! Typed access to the fixtures recorded from the Go engine.
//!
//! `tools/go-fixtures` runs the Go engine's test suite with recorders attached and
//! writes what they saw to `fixtures/go`. This crate reads those files so Rust tests
//! can replay them. The format is described in `fixtures/go/README.md`.
//!
//! The crate is test support only: it reads from disk and is never published.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// The directory that holds the Go fixtures, `fixtures/go` at the workspace root.
pub fn default_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/go")
}

/// An error while loading fixtures.
#[derive(Debug)]
pub enum Error {
    /// A file could not be read.
    Io(PathBuf, io::Error),
    /// A file did not match the expected format.
    Parse(PathBuf, serde_json::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Error::Parse(p, e) => write!(f, "{}: {e}", p.display()),
        }
    }
}

impl std::error::Error for Error {}

/// `MANIFEST.json`: where and when the fixtures were recorded.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Remote URL of the Go repository.
    pub go_repo: String,
    /// Commit of the Go repository the fixtures were recorded at.
    pub commit: String,
    /// Whether the Go checkout had uncommitted changes.
    pub dirty: bool,
    /// Go toolchain version used for the run.
    pub go_version: String,
    /// Package patterns passed to `go test`.
    pub packages: Vec<String>,
    /// When the export ran, RFC 3339.
    pub exported_at: String,
    /// Totals by kind.
    pub counts: BTreeMap<String, u64>,
    /// Go tests that failed during the run; their fixtures are marked `fail`.
    pub failed_tests: Option<Vec<String>>,
}

/// One Go test's HTTP traffic, from `http/<package>/<Test>.json`.
#[derive(Debug, Deserialize)]
pub struct HttpFixture {
    /// Go package path relative to the module, such as `internal/handlers`.
    pub package: String,
    /// Top-level Go test name.
    pub test: String,
    /// `pass`, `fail`, `skip` or `unknown`. Only `pass` fixtures describe correct behaviour.
    pub outcome: String,
    /// One entry per router the test built, in creation order.
    pub sessions: Vec<HttpSession>,
}

/// The requests one router served, starting from a fresh engine.
#[derive(Debug, Deserialize)]
pub struct HttpSession {
    /// Id of the database state taken just before the first request.
    pub db_before: Option<String>,
    /// Why the state could not be taken, when it could not.
    pub db_error: Option<String>,
    /// Requests and responses in the order the router finished them.
    pub exchanges: Vec<Exchange>,
}

/// One request and the response the Go engine gave.
#[derive(Debug, Deserialize)]
pub struct Exchange {
    /// The echo route pattern that matched, such as `/api/moul/:name/records/:id`.
    pub route: String,
    /// The request as received.
    pub request: Request,
    /// The response as sent.
    pub response: Response,
}

/// A recorded HTTP request.
#[derive(Debug, Deserialize)]
pub struct Request {
    /// HTTP method.
    pub method: String,
    /// URL path.
    pub path: String,
    /// Raw query string, without `?`.
    pub query: String,
    /// Header values by canonical name.
    pub headers: BTreeMap<String, Vec<String>>,
    /// Body, if any.
    pub body: Option<Body>,
}

/// A recorded HTTP response.
#[derive(Debug, Deserialize)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// Header values by canonical name.
    pub headers: BTreeMap<String, Vec<String>>,
    /// Bytes written, which can exceed the recorded body.
    pub size: i64,
    /// Body, if any.
    pub body: Option<Body>,
    /// True when only the start of the body was kept.
    #[serde(default)]
    pub body_truncated: bool,
}

/// A request or response body. Exactly one field is set.
#[derive(Debug, Deserialize)]
pub struct Body {
    /// Parsed JSON body.
    pub json: Option<Value>,
    /// UTF-8 text body.
    pub text: Option<String>,
    /// Binary body, base64.
    pub base64: Option<String>,
    /// Set when the body was too large to keep; `len` and `sha256` describe it.
    #[serde(rename = "$omitted", default)]
    pub omitted: bool,
    /// Length of an omitted body.
    pub len: Option<u64>,
    /// SHA-256 of an omitted body, hex.
    pub sha256: Option<String>,
}

/// One deduplicated call to a rule function, from `rules/<kind>.jsonl`.
#[derive(Debug, Deserialize)]
pub struct RuleCase {
    /// Hash of input and output.
    pub id: String,
    /// `package.Test` names that made this exact call; `(server)` means inside a request.
    pub sources: Vec<String>,
    /// The arguments, as JSON.
    pub input: Value,
    /// The result, as JSON.
    pub output: Value,
}

/// The rule functions that are recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// `rules.EvaluateRule`: does a rule allow this record for this caller.
    Evaluate,
    /// `rules.BuildFilterSQLWithOptions`: a filter or rule compiled to a SQL `WHERE`.
    FilterSql,
    /// `rules.BuildSortSQLWithOptions`: a sort string compiled to `ORDER BY` terms.
    SortSql,
}

impl RuleKind {
    /// Every kind.
    pub const ALL: [RuleKind; 3] = [RuleKind::Evaluate, RuleKind::FilterSql, RuleKind::SortSql];

    fn file(self) -> &'static str {
        match self {
            RuleKind::Evaluate => "evaluate.jsonl",
            RuleKind::FilterSql => "filter_sql.jsonl",
            RuleKind::SortSql => "sort_sql.jsonl",
        }
    }
}

/// A database captured during the Go run, from `db/<id>.json`.
#[derive(Debug, Deserialize)]
pub struct DbState {
    /// Every `sqlite_master` entry with SQL, tables first.
    pub schema: Vec<SchemaObject>,
    /// Rows by table name.
    pub tables: BTreeMap<String, Value>,
}

/// One `sqlite_master` entry.
#[derive(Debug, Deserialize)]
pub struct SchemaObject {
    /// `table`, `index`, `trigger` or `view`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Object name.
    pub name: String,
    /// Table the object belongs to.
    pub table: String,
    /// The `CREATE` statement.
    pub sql: String,
}

impl DbState {
    /// Recreates this state in the `main` schema of `conn`, which should be
    /// empty: tables, then rows, then indexes, triggers and views. Blobs
    /// recorded as `{"$bytes": base64}` are decoded.
    pub fn load_into(&self, conn: &rusqlite::Connection) -> rusqlite::Result<()> {
        let order = |k: &str| match k {
            "table" => 0,
            "index" => 1,
            "trigger" => 2,
            _ => 3,
        };
        let mut objects: Vec<&SchemaObject> = self.schema.iter().collect();
        objects.sort_by_key(|o| order(&o.kind));
        for o in objects.iter().filter(|o| o.kind == "table") {
            conn.execute_batch(&o.sql)?;
        }
        for (table, data) in &self.tables {
            let columns: Vec<String> = data["columns"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let Some(rows) = data["rows"].as_array() else {
                continue;
            };
            if columns.is_empty() {
                continue;
            }
            let quoted: Vec<String> = columns
                .iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect();
            let marks: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "INSERT INTO \"{}\" ({}) VALUES ({})",
                table.replace('"', "\"\""),
                quoted.join(", "),
                marks.join(", ")
            );
            let mut stmt = conn.prepare(&sql)?;
            for row in rows {
                let values: Vec<rusqlite::types::Value> = row
                    .as_array()
                    .map(|r| r.iter().map(sql_value).collect())
                    .unwrap_or_default();
                stmt.execute(rusqlite::params_from_iter(values))?;
            }
        }
        for o in objects.iter().filter(|o| o.kind != "table") {
            conn.execute_batch(&o.sql)?;
        }
        Ok(())
    }
}

fn sql_value(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as V;
    match v {
        Value::Null => V::Null,
        Value::Bool(b) => V::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => V::Integer(i),
            None => V::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => V::Text(s.clone()),
        Value::Object(o) => match o.get("$bytes").and_then(|b| b.as_str()) {
            Some(b64) => V::Blob(decode_base64(b64)),
            None => V::Text(v.to_string()),
        },
        Value::Array(_) => V::Text(v.to_string()),
    }
}

/// Decodes standard base64 with or without padding.
pub fn decode_base64(s: &str) -> Vec<u8> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter_map(val) {
        acc = (acc << 6) | c;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// A fixtures directory.
#[derive(Debug, Clone)]
pub struct Fixtures {
    dir: PathBuf,
}

impl Default for Fixtures {
    fn default() -> Self {
        Self::open(default_dir())
    }
}

impl Fixtures {
    /// Use the fixtures in `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory being read.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read `MANIFEST.json`.
    pub fn manifest(&self) -> Result<Manifest, Error> {
        read_json(&self.dir.join("MANIFEST.json"))
    }

    /// Paths of every HTTP fixture, sorted.
    pub fn http_paths(&self) -> Result<Vec<PathBuf>, Error> {
        let mut out = Vec::new();
        walk(&self.dir.join("http"), &mut out)?;
        out.retain(|p| p.extension().is_some_and(|e| e == "json"));
        out.sort();
        Ok(out)
    }

    /// Read one HTTP fixture.
    pub fn http(&self, path: &Path) -> Result<HttpFixture, Error> {
        read_json(path)
    }

    /// Read every recorded call of one rule function.
    pub fn rules(&self, kind: RuleKind) -> Result<Vec<RuleCase>, Error> {
        let path = self.dir.join("rules").join(kind.file());
        let text = fs::read_to_string(&path).map_err(|e| Error::Io(path.clone(), e))?;
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).map_err(|e| Error::Parse(path.clone(), e)))
            .collect()
    }

    /// Read a database state by id.
    pub fn db(&self, id: &str) -> Result<DbState, Error> {
        read_json(&self.dir.join("db").join(format!("{id}.json")))
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let bytes = fs::read(path).map_err(|e| Error::Io(path.to_owned(), e))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Parse(path.to_owned(), e))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let entries = fs::read_dir(dir).map_err(|e| Error::Io(dir.to_owned(), e))?;
    for entry in entries {
        let path = entry.map_err(|e| Error::Io(dir.to_owned(), e))?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}
