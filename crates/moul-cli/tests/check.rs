//! `moul check` on schema files and databases.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use moul_core::{Config, Engine, MemoryBlobs, Request, SeededHost};
use moul_store::Store;
use serde_json::{Value, json};

/// What one run of `moul check` printed, and its exit code.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| panic!("{e}: {}", self.stdout))
    }
}

fn check_in(cwd: &Path, args: &[&str]) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_moul"))
        .arg("check")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    Run {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("moul-check")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Rules that let only signed-in people write: no warning.
fn closed() -> Value {
    json!({
        "listRule": "",
        "viewRule": "",
        "createRule": "@request.auth.id != ''",
        "updateRule": "owner = @request.auth.id",
        "deleteRule": "owner = @request.auth.id",
    })
}

fn teams() -> Value {
    json!({
        "name": "teams",
        "fields": [{"name": "name", "type": "text"}, {"name": "owner", "type": "text"}],
        "rules": closed(),
    })
}

fn tasks(rules: Value) -> Value {
    json!({
        "name": "tasks",
        "fields": [
            {"name": "title", "type": "text"},
            {"name": "owner", "type": "text"},
            {"name": "team", "type": "relation",
             "relationConfig": {"targetMoul": "teams", "cardinality": "1:N"}},
        ],
        "rules": rules,
    })
}

fn schema(collections: Vec<Value>) -> Value {
    json!({"format": 1, "collections": collections})
}

/// Writes `file` as `name` in `dir`.
fn write(dir: &Path, name: &str, file: &Value) {
    std::fs::write(dir.join(name), serde_json::to_string_pretty(file).unwrap()).unwrap();
}

#[test]
fn a_valid_schema_passes() {
    let dir = scratch("valid");
    let rules = json!({
        "listRule": "owner = @request.auth.id || team.owner = @request.auth.id",
        "viewRule": "owner = @request.auth.id && title:lower != ''",
        "createRule": "@request.auth.id != '' && @request.body.owner = @request.auth.id",
        "updateRule": "owner = @request.auth.id && @request.body.owner:changed = false",
        "deleteRule": "owner = @request.auth.id",
        "subscribeRule": "@collection.teams.owner = @request.auth.id",
    });
    write(
        &dir,
        "moul.schema.json",
        &schema(vec![teams(), tasks(rules)]),
    );
    // With no path, moul.schema.json in the current folder.
    let r = check_in(&dir, &[]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    assert_eq!(r.stdout, "moul.schema.json is valid: 2 collections\n");
    let r = check_in(&dir, &["moul.schema.json", "--strict"]);
    assert_eq!(r.code, 0, "{}", r.stdout);
}

#[test]
fn rules_are_compiled_against_their_collection() {
    let dir = scratch("rules");
    let rules = json!({
        "listRule": "ownr = @request.auth.id",
        "viewRule": "team.owner = @request.auth.id",
        // `@request.body` is the record being written: not a field.
        "createRule": "@request.body.owner = @request.auth.id && titel != ''",
        "updateRule": "owner = @request.auth.id && (title = 'x'",
        "deleteRule": "owner = @request.auth.id",
        "subscribeRule": "@collection.teams.ownr = @request.auth.id",
    });
    write(&dir, "app.json", &schema(vec![teams(), tasks(rules)]));
    let r = check_in(&dir, &["app.json"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    let at = "collections[1] (tasks).rules";
    for line in [
        format!(r#"error: {at}.listRule: invalid field "ownr""#),
        format!(
            r#"error: {at}.viewRule: "team.owner" follows the relation "team", which only a list rule can do"#
        ),
        format!(r#"error: {at}.createRule: invalid field "titel""#),
        format!(r#"error: {at}.subscribeRule: invalid field "ownr" in "@collection.teams.ownr""#),
    ] {
        assert!(
            r.stdout.contains(&format!("{line}\n")),
            "{line}\n{}",
            r.stdout
        );
    }
    assert!(
        r.stdout
            .contains(&format!("error: {at}.updateRule: failed to compile rule")),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("@request.body.owner"), "{}", r.stdout);
    assert!(r.stdout.ends_with("5 errors in app.json\n"), "{}", r.stdout);
}

#[test]
fn a_relation_needs_its_target() {
    let dir = scratch("relation");
    write(&dir, "app.json", &schema(vec![tasks(closed())]));
    let r = check_in(&dir, &["app.json"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(
        r.stdout.starts_with(
            "error: collections[0] (tasks).fields[2] (team): relation target \"teams\" is not a collection\n"
        ),
        "{}",
        r.stdout
    );
}

#[test]
fn the_file_and_each_definition_are_read_first() {
    let dir = scratch("file");
    std::fs::write(
        dir.join("broken.json"),
        "{\"format\": 1, \"collections\": [\n",
    )
    .unwrap();
    let r = check_in(&dir, &["broken.json"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(
        r.stdout
            .starts_with("error: line 2, column 0: not JSON: EOF while parsing a list\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.ends_with("1 error in broken.json\n"),
        "{}",
        r.stdout
    );

    write(
        &dir,
        "future.json",
        &json!({"format": 2, "collections": []}),
    );
    let r = check_in(&dir, &["future.json"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(
        r.stdout.starts_with(
            "error: format: the schema file has format 2; this engine reads format 1 and older\n"
        ),
        "{}",
        r.stdout
    );

    // Every bad definition is reported, and a repeated name, which
    // applying would read as a change to the first.
    let file = schema(vec![
        teams(),
        json!({"name": "notes", "fields": [{"name": "Bad Name", "type": "text"}]}),
        json!({"name": "Teams"}),
        json!({"id": "not an id!", "name": "posts", "rules": closed()}),
    ]);
    write(&dir, "defs.json", &file);
    let r = check_in(&dir, &["defs.json"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert_eq!(
        r.stdout,
        "error: collections[1] (notes): field name \"Bad Name\" must be camelCase (e.g. \"authorId\")\n\
         error: collections[2] (Teams): collections[0] (teams) has the same name\n\
         2 errors in defs.json\n"
    );
    // What only applying finds, blamed on its collection.
    let file = schema(vec![
        teams(),
        json!({"id": "not an id!", "name": "posts", "rules": closed(),
               "fields": [{"name": "owner", "type": "text"}]}),
    ]);
    write(&dir, "apply.json", &file);
    let r = check_in(&dir, &["apply.json"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(
        r.stdout.starts_with(
            "error: collections[1] (posts): Collection id must be 1 to 64 letters, digits, '-' or '_'\n"
        ),
        "{}",
        r.stdout
    );
}

#[test]
fn open_write_rules_warn_and_strict_fails_on_them() {
    let dir = scratch("warnings");
    let open = json!({"listRule": "", "viewRule": "", "createRule": "",
                      "updateRule": "", "deleteRule": ""});
    // An auth collection's empty list and view rules mean "own account
    // only" once applied: no warning for those.
    let users = json!({"name": "users", "type": "auth", "rules": {"createRule": ""}});
    write(&dir, "app.json", &schema(vec![teams(), tasks(open), users]));
    let r = check_in(&dir, &["app.json"]);
    assert_eq!(r.code, 0, "{}", r.stdout);
    let at = "collections[1] (tasks).rules";
    assert_eq!(
        r.stdout,
        format!(
            "warning: {at}.createRule: anyone can create records in tasks, guests included: set createRule\n\
             warning: {at}.updateRule: anyone can change records of tasks, guests included: set updateRule\n\
             warning: {at}.deleteRule: anyone can delete records of tasks, guests included: set deleteRule\n\
             warning: collections[2] (users).rules.createRule: anyone can create accounts in users, guests included: set createRule\n\
             app.json is valid: 3 collections, 4 warnings\n"
        )
    );
    let r = check_in(&dir, &["app.json", "--strict"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(
        r.stdout.ends_with("4 warnings in app.json\n"),
        "{}",
        r.stdout
    );
}

#[test]
fn json_output_has_a_stable_shape() {
    let dir = scratch("json");
    let rules = json!({"listRule": "ownr = ''", "viewRule": "", "createRule": "",
                       "updateRule": "", "deleteRule": ""});
    write(&dir, "app.json", &schema(vec![teams(), tasks(rules)]));
    let r = check_in(&dir, &["--json", "app.json"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    let report = r.json();
    let keys: Vec<&String> = report.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 5, "{report}");
    assert_eq!(report["ok"], false);
    assert_eq!(report["file"], "app.json");
    assert_eq!(report["collections"], 2);
    assert_eq!(
        report["errors"],
        json!([{"location": "collections[1] (tasks).rules.listRule",
                "message": "invalid field \"ownr\""}])
    );
    let warnings = report["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 3, "{report}");
    assert_eq!(
        warnings[2],
        json!({"location": "collections[1] (tasks).rules.deleteRule",
               "message": "anyone can delete records of tasks, guests included: set deleteRule"})
    );
    // The JSON object starts with "ok", for anyone reading it.
    assert!(r.stdout.starts_with("{\n  \"ok\": false,"), "{}", r.stdout);

    // `ok` follows the exit code: warnings fail it only with --strict.
    write(&dir, "open.json", &schema(vec![tasks(json!({})), teams()]));
    let r = check_in(&dir, &["open.json", "--json"]);
    assert_eq!(
        (r.code, r.json()["ok"].clone()),
        (0, json!(true)),
        "{}",
        r.stdout
    );
    let r = check_in(&dir, &["open.json", "--json", "--strict"]);
    assert_eq!(
        (r.code, r.json()["ok"].clone()),
        (1, json!(false)),
        "{}",
        r.stdout
    );
    assert_eq!(r.json()["errors"], json!([]));
}

#[test]
fn checks_a_database_made_by_the_engine_without_changing_it() {
    let dir = scratch("database");
    let mut engine = Engine::open(
        Store::open_in_memory().unwrap(),
        SeededHost::new(1, 1_794_961_964_000),
        MemoryBlobs::new(),
        Config::default(),
    )
    .unwrap();
    let mut rules = closed();
    rules["deleteRule"] = json!("");
    let r =
        engine.handle(Request::post("/api/schema", schema(vec![teams(), tasks(rules)])).owner());
    assert_eq!(r.status, 200, "{:?}", r.body);
    let db = dir.join("app.db");
    std::fs::write(&db, engine.store().export().unwrap()).unwrap();
    let before = std::fs::read(&db).unwrap();

    // Collections are placed as `moul schema` writes them: by name.
    let r = check_in(&dir, &["app.db"]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    assert_eq!(
        r.stdout,
        "warning: collections[0] (tasks).rules.deleteRule: anyone can delete records of tasks, guests included: set deleteRule\n\
         app.db is valid: 2 collections, 1 warning\n"
    );
    let r = check_in(&dir, &["app.db", "--strict", "--json"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert_eq!(r.json()["collections"], 2);

    // The engine refuses a broken rule, but a database the Go engine made
    // can hold one.
    let r = engine.handle(
        Request::patch(
            "/api/moul/tasks",
            json!({"rules": {"listRule": "ownr = ''"}}),
        )
        .owner(),
    );
    assert_eq!(r.status, 400, "{:?}", r.body);
    let broken = dir.join("broken.db");
    std::fs::write(&broken, engine.store().export().unwrap()).unwrap();
    rusqlite::Connection::open(&broken)
        .unwrap()
        .execute(
            "UPDATE _moul SET rules = json_set(rules, '$.listRule', ?1) WHERE name = 'tasks'",
            ["ownr = ''"],
        )
        .unwrap();
    let r = check_in(&dir, &["broken.db"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(
        r.stdout
            .starts_with("error: collections[0] (tasks).rules.listRule: invalid field \"ownr\"\n"),
        "{}",
        r.stdout
    );

    assert!(
        std::fs::read(&db).unwrap() == before,
        "the database changed"
    );
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["app.db", "broken.db"]);
}

#[test]
fn what_cannot_be_checked_exits_2() {
    let dir = scratch("usage");
    let r = check_in(&dir, &[]);
    assert_eq!(r.code, 2);
    assert_eq!(r.stdout, "");
    assert!(
        r.stderr.contains("no moul.schema.json here"),
        "{}",
        r.stderr
    );
    let r = check_in(&dir, &["missing.json", "--json"]);
    assert_eq!((r.code, r.stdout.as_str()), (2, ""));
    assert!(
        r.stderr.starts_with("moul: cannot read missing.json"),
        "{}",
        r.stderr
    );
    let r = check_in(&dir, &["--frob"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("unknown option --frob"), "{}", r.stderr);
    // A SQLite file that is not a Moul database.
    let other = dir.join("other.db");
    rusqlite::Connection::open(&other)
        .unwrap()
        .execute_batch("CREATE TABLE t (x)")
        .unwrap();
    let r = check_in(&dir, &["other.db"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("not a Moul database"), "{}", r.stderr);
}
