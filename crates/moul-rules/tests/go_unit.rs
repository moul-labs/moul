//! The Go engine's own tests of its rule package (`internal/rules/*_test.go`),
//! ported. The untrusted-filter tests run the compiled SQL on SQLite, as the
//! Go tests do.

mod common;

use common::{GoDb, NOW_MS, list_ids};
use moul_rules::{
    EvalContext, FilterOptions, Map, SortOptions, check, evaluate, filter_sql, is_secret_field,
    is_valid_field, owner_fields, sort_sql, translate,
};
use moul_store::schema::{Collection, CollectionKind, Field, FieldType, Relation};
use serde_json::{Value, json};

fn map(v: Value) -> Map {
    v.as_object().cloned().unwrap_or_default()
}

fn field(name: &str, kind: FieldType) -> Field {
    Field::new(name, kind)
}

fn relation(name: &str, target: &str) -> Field {
    let mut f = Field::new(name, FieldType::Relation);
    f.relation = Some(Relation {
        target_moul: target.into(),
        cardinality: "1:N".into(),
        on_delete: String::new(),
    });
    f
}

fn collection(name: &str, kind: CollectionKind, fields: Vec<Field>, list_rule: &str) -> Collection {
    let mut c = Collection::new(name, kind);
    c.fields = fields;
    c.rules.list_rule = list_rule.into();
    c
}

// ------------------------------------------------------------ evaluator_test.go

#[test]
fn evaluate_rule_pocketbase_syntax() {
    let db = GoDb::new();
    db.conn
        .execute_batch(
            "CREATE TABLE user_roles (id TEXT PRIMARY KEY, user_id TEXT, role TEXT);
             CREATE TABLE posts (id TEXT PRIMARY KEY, title TEXT, author_id TEXT);
             INSERT INTO user_roles VALUES ('ur1', 'u1', 'admin');
             INSERT INTO posts VALUES ('post1', 'Lorem Ipsum', 'u1');",
        )
        .unwrap();
    let mut posts = collection(
        "posts",
        CollectionKind::Base,
        vec![
            field("title", FieldType::Text),
            field("authorId", FieldType::Text),
        ],
        "",
    );
    posts.id = "moul-posts".into();
    db.register(&posts);

    struct Case {
        name: &'static str,
        rule: &'static str,
        auth: Option<Value>,
        record: Value,
        request: Option<Value>,
        expected: bool,
    }
    let case = |name, rule, expected| Case {
        name,
        rule,
        auth: None,
        record: json!({}),
        request: None,
        expected,
    };
    let cases = vec![
        case("Empty rule", "", true),
        Case {
            record: json!({"status": "active"}),
            ..case(
                "Basic equality operator translation",
                "status = 'active'",
                true,
            )
        },
        Case {
            auth: Some(json!({"id": "u1"})),
            ..case("Basic request auth path", "@request.auth.id != ''", true)
        },
        Case {
            request: Some(json!({"query": {"search": "golang"}})),
            ..case("Request query check", "@request.query.search != ''", true)
        },
        Case {
            request: Some(json!({"headers": {"x_custom_header": "val"}})),
            ..case(
                "Request header check (normalization)",
                "@request.headers.x_custom_header = 'val'",
                true,
            )
        },
        Case {
            record: json!({"title": "Lorem Ipsum"}),
            ..case("Like (~) operator translation", "title ~ 'Lorem%'", true)
        },
        Case {
            record: json!({"title": "Lorem Ipsum dolor"}),
            ..case(
                "Like (~) operator default wildcard wrapping",
                "title ~ 'Ipsum'",
                true,
            )
        },
        Case {
            record: json!({"tags": ["other", "test"]}),
            ..case("Wildcard operator ?=", "tags ?= 'test'", true)
        },
        Case {
            record: json!({"createdAt": "2020-01-01 00:00:00Z"}),
            ..case("Datetime macro @now comparison", "createdAt < @now", true)
        },
        Case {
            record: json!({"title": "LOREM"}),
            ..case("Modifier :lower", "title:lower = 'lorem'", true)
        },
        Case {
            record: json!({"tags": ["a", "b"]}),
            ..case("Modifier :length on slice", "tags:length = 2", true)
        },
        Case {
            request: Some(json!({"body": {"title": "hello"}})),
            ..case(
                "Modifier :isset on request body",
                "@request.body.role:isset = false",
                true,
            )
        },
        Case {
            record: json!({"role": "user"}),
            request: Some(json!({"body": {"role": "user"}})),
            ..case(
                "Modifier :changed on request body",
                "@request.body.role:changed = false",
                true,
            )
        },
        Case {
            record: json!({"tags": ["tag1", "tag2"]}),
            ..case("Modifier :each with like", "tags:each ~ 'tag'", true)
        },
        Case {
            record: json!({"tags": ["tag1", "other"]}),
            ..case("Modifier :each mismatch", "tags:each ~ 'tag'", false)
        },
        case(
            "Function geoDistance",
            "geoDistance(23.32, 42.69, 23.33, 42.70) < 5",
            true,
        ),
        case(
            "Function strftime",
            "strftime('%Y-%m', '2026-07-19 12:34:56') = '2026-07'",
            true,
        ),
        Case {
            auth: Some(json!({"id": "u1"})),
            ..case(
                "Grouped @collection query check (Match)",
                "@collection.user_roles.user_id = @request.auth.id && @collection.user_roles.role = 'admin'",
                true,
            )
        },
        Case {
            auth: Some(json!({"id": "u1"})),
            ..case(
                "Grouped @collection query check (Mismatch)",
                "@collection.user_roles.user_id = @request.auth.id && @collection.user_roles.role = 'editor'",
                false,
            )
        },
        // Not in the Go table, but the reason `get(` is set up above.
        Case {
            record: json!({"post": "post1"}),
            ..case(
                "Function get",
                "isset(get('posts', post), 'title') && get('posts', 'nope') = null",
                true,
            )
        },
        case(
            "Function exists",
            "exists('posts', 'author_id = ?', 'u1') && exists('posts', 'id = ?') = false",
            true,
        ),
    ];
    for c in cases {
        let auth = c.auth.map(map);
        let record = map(c.record);
        let request = c.request.map(map);
        let ctx = EvalContext {
            auth: auth.as_ref(),
            record: &record,
            request: request.as_ref(),
            db: Some(&db),
            now_ms: NOW_MS,
        };
        match evaluate(c.rule, &ctx) {
            Ok(allowed) => assert_eq!(allowed, c.expected, "{}", c.name),
            Err(e) => panic!("{}: unexpected error: {e}", c.name),
        }
    }
}

#[test]
fn evaluate_reports_errors_like_go() {
    let record = map(json!({"status": "active"}));
    let ctx = EvalContext {
        auth: None,
        record: &record,
        request: None,
        db: None,
        now_ms: NOW_MS,
    };
    let err = |rule| evaluate(rule, &ctx).unwrap_err().to_string();
    assert_eq!(
        err("invalid == (&&"),
        "failed to compile rule 'invalid == (&&': unexpected token Operator(\"&&\") (1:14)\n | invalid == ( && \n | .............^"
    );
    assert_eq!(
        err("status:upper = 'x'"),
        "failed to translate rule: unsupported modifier :upper"
    );
    assert_eq!(
        err("status = 1"),
        "failed to compile rule 'status = 1': invalid operation: == (mismatched types string and int) (1:8)\n | status == 1\n | .......^"
    );
    assert_eq!(
        err("status"),
        "failed to compile rule 'status': expected bool, but got string"
    );
    // An undefined name is nil, and a nil result is false.
    assert_eq!(evaluate("nothing", &ctx), Ok(false));
}

#[test]
fn check_reports_syntax_errors_only() {
    assert!(check("").is_ok());
    assert!(check("owner = @request.auth.id && (status = 'a' || status ~ 'b')").is_ok());
    // Types depend on the record, so a mismatch is not a syntax error.
    assert!(check("status = 1").is_ok());
    assert_eq!(
        check("a = 'x").unwrap_err().message(),
        "failed to translate rule: unterminated string literal starting at position 4"
    );
    assert!(
        check("a = (b")
            .unwrap_err()
            .message()
            .starts_with("failed to compile rule 'a = (b': unexpected token EOF")
    );
}

#[test]
fn translate_shows_the_expression() {
    assert_eq!(
        translate("title:lower ~ 'x' && @request.auth.id != ''").unwrap(),
        "like(title:lower, \"x\") && request.auth.id != \"\""
    );
    assert_eq!(translate("").unwrap(), "");
}

// ------------------------------------------------------------ sql_translator_test.go

fn posts() -> Collection {
    collection(
        "posts",
        CollectionKind::Base,
        vec![
            field("title", FieldType::Text),
            field("price", FieldType::Number),
            field("author_id", FieldType::Text),
            field("active", FieldType::Bool),
        ],
        "",
    )
}

#[test]
fn build_sort_sql() {
    let posts = posts();
    let cases: &[(&str, Option<&[&str]>)] = &[
        ("", Some(&[])),
        ("-created,title", Some(&["created DESC", "title ASC"])),
        ("+price,-id", Some(&["price ASC", "id DESC"])),
        ("@random", Some(&["RANDOM()"])),
        ("invalid_col", None),
    ];
    for (input, expected) in cases {
        let got = sort_sql(input, &posts, &SortOptions::default());
        match expected {
            Some(want) => assert_eq!(got.unwrap(), *want, "{input}"),
            None => assert!(got.is_err(), "{input}"),
        }
    }
}

#[test]
fn build_filter_sql() {
    let posts = posts();
    let auth = map(json!({"id": "user123", "email": "user@test.com"}));
    let opts = FilterOptions::default();
    let cases: &[(&str, Option<&Map>, &str, Value)] = &[
        (
            "title = 'Hello'",
            None,
            "title = :p1",
            json!({"p1": "Hello"}),
        ),
        (
            "price > 50 && active = true",
            None,
            "(price > :p1 AND active = :p2)",
            json!({"p1": 50, "p2": true}),
        ),
        (
            "title ~ 'test'",
            None,
            "title LIKE :p1",
            json!({"p1": "%test%"}),
        ),
        (
            "author_id = @request.auth.id",
            Some(&auth),
            "author_id = :p1",
            json!({"p1": "user123"}),
        ),
        (
            "@collection.posts.author_id = id",
            None,
            "EXISTS (SELECT 1 FROM `posts` AS _sub WHERE _sub.`author_id` = `posts`.`id`)",
            json!({}),
        ),
    ];
    for (filter, auth, sql, params) in cases {
        let f = filter_sql(filter, &posts, *auth, None, &opts)
            .unwrap()
            .unwrap();
        assert_eq!(f.sql, *sql, "{filter}");
        assert_eq!(
            Value::Object(f.params.into_iter().collect()),
            *params,
            "{filter}"
        );
    }
    assert_eq!(filter_sql("", &posts, None, None, &opts), Ok(None));
    assert!(filter_sql("unknown_column = 123", &posts, None, None, &opts).is_err());
}

#[test]
fn build_filter_sql_cloak() {
    let mut ssn = field("ssn", FieldType::Cloak);
    ssn.searchable = true;
    let patients = collection(
        "patients",
        CollectionKind::Base,
        vec![ssn, field("secretNotes", FieldType::Cloak)],
        "",
    );
    let hash = |s: &str| format!("hmac:{s}");
    let opts = FilterOptions {
        cloak_hash: Some(&hash),
        ..Default::default()
    };
    let sql = |filter| filter_sql(filter, &patients, None, None, &opts);

    let f = sql(r#"ssn = "123-45-6789""#).unwrap().unwrap();
    assert_eq!(f.sql, "`ssnHash` = :p1");
    assert_eq!(f.params, [("p1".to_owned(), json!("hmac:123-45-6789"))]);
    let f = sql(r#"ssn != "123-45-6789""#).unwrap().unwrap();
    assert_eq!(f.sql, "`ssnHash` != :p1");
    assert_eq!(f.params, [("p1".to_owned(), json!("hmac:123-45-6789"))]);
    assert_eq!(
        sql(r#"ssn ~ "123""#).unwrap_err().message(),
        "cloak field \"ssn\" only supports '=' and '!=' operators"
    );
    assert!(sql(r#"ssn > "123""#).is_err());
    assert_eq!(
        sql(r#"secretNotes = "private""#).unwrap_err().message(),
        "cloak field \"secretNotes\" is not searchable"
    );
}

#[test]
fn owner_fields_and_field_checks() {
    assert_eq!(owner_fields("owner = @request.auth.id"), ["owner"]);
    assert_eq!(
        owner_fields(
            "@request.auth.id = author || coauthor == auth.id && id = @auth.id || author = @auth.id"
        ),
        ["author", "coauthor"]
    );
    assert!(owner_fields("owner != @request.auth.id && true = @request.auth.id").is_empty());
    assert!(owner_fields("owner = 'unterminated").is_empty());

    let users = collection(
        "users",
        CollectionKind::Auth,
        vec![field("team", FieldType::Relation)],
        "",
    );
    assert!(is_valid_field("createdAt", &users));
    assert!(is_valid_field("team", &users));
    assert!(is_valid_field("team.name", &users));
    assert!(!is_valid_field("Team", &users));
    assert!(!is_valid_field("nope.name", &users));
    assert!(is_secret_field("passwordHash", &users));
    assert!(is_secret_field("TOKENKEY", &users));
    assert!(is_secret_field("email", &users));
    assert!(!is_secret_field("email", &posts()));
}

// ------------------------------------------------------------ untrusted_filter_test.go

struct Fixture {
    db: GoDb,
    users: Collection,
    garages: Collection,
    builds: Collection,
}

impl Fixture {
    fn load(&self, name: &str) -> Option<Collection> {
        [&self.users, &self.garages, &self.builds]
            .into_iter()
            .find(|c| c.name == name)
            .cloned()
    }
}

/// Users (auth), garages (only the owner may list them) and builds (public,
/// with a relation to garages and one to users).
fn untrusted_fixture() -> Fixture {
    let mut garages = collection(
        "garages",
        CollectionKind::Base,
        vec![field("vin", FieldType::Text), relation("owner", "users")],
        "owner = @request.auth.id",
    );
    garages.id = "m2".into();
    let fx = Fixture {
        db: GoDb::new(),
        users: collection(
            "users",
            CollectionKind::Auth,
            vec![field("displayName", FieldType::Text)],
            "",
        ),
        garages,
        builds: collection(
            "builds",
            CollectionKind::Base,
            vec![
                field("title", FieldType::Text),
                relation("garage", "garages"),
                relation("author", "users"),
            ],
            "",
        ),
    };
    fx.db
        .conn
        .execute_batch(
            "CREATE TABLE users (id TEXT PRIMARY KEY, createdAt TEXT, updatedAt TEXT, username TEXT, email TEXT,
                 passwordHash TEXT, tokenKey TEXT, displayName TEXT);
             CREATE TABLE garages (id TEXT PRIMARY KEY, createdAt TEXT, updatedAt TEXT, vin TEXT, owner TEXT);
             CREATE TABLE builds (id TEXT PRIMARY KEY, createdAt TEXT, updatedAt TEXT, title TEXT, garage TEXT, author TEXT);
             INSERT INTO users (id, username, email) VALUES ('alice', 'alice', 'alice@example.com'), ('bob', 'bob', 'bob@example.com');
             INSERT INTO garages (id, vin, owner) VALUES ('g_alice', 'VIN-ALICE', 'alice'), ('g_bob', 'VIN-BOB', 'bob');
             INSERT INTO builds (id, title, garage, author) VALUES ('b1', 'Alice build', 'g_alice', 'alice'),
                 ('b2', 'Bob build', 'g_bob', 'bob');",
        )
        .unwrap();
    fx
}

#[test]
fn untrusted_filter_param_prefix_keeps_rule_values() {
    let fx = untrusted_fixture();
    let auth = map(json!({"id": "alice"}));
    let load = |n: &str| fx.load(n);
    let rule = filter_sql(
        &fx.garages.rules.list_rule,
        &fx.garages,
        Some(&auth),
        None,
        &FilterOptions {
            param_prefix: "rule_",
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    let filter = filter_sql(
        "owner = 'bob'",
        &fx.garages,
        Some(&auth),
        None,
        &FilterOptions {
            param_prefix: "filter_",
            untrusted: true,
            load: Some(&load),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    let combined = moul_rules::SqlFilter {
        sql: format!("({}) AND ({})", rule.sql, filter.sql),
        params: rule.params.into_iter().chain(filter.params).collect(),
    };
    let got = list_ids(&fx.db.conn, "garages", Some(&combined));
    assert!(
        got.is_empty(),
        "filter overwrote the list rule: alice listed {got:?}"
    );
}

#[test]
fn untrusted_filter_rejects_collection_and_secrets() {
    let fx = untrusted_fixture();
    let load = |n: &str| fx.load(n);
    let opts = FilterOptions {
        untrusted: true,
        load: Some(&load),
        ..Default::default()
    };
    let cases = [
        (
            "collection subquery",
            &fx.builds,
            "@collection._rootUsers.passwordHash ~ '$2a'",
        ),
        (
            "settings oracle",
            &fx.builds,
            "@collection._settings.value ~ 'AKIA'",
        ),
        ("password hash", &fx.users, "passwordHash ~ '$2a'"),
        ("password modifier", &fx.users, "passwordHash:length > 10"),
        ("auth email", &fx.users, "email ~ 'alice'"),
        ("email via relation", &fx.builds, "author.email ~ 'alice'"),
        ("token key", &fx.users, "tokenKey != ''"),
        ("nested path", &fx.builds, "garage.owner.email ~ 'a'"),
    ];
    for (name, c, filter) in cases {
        assert!(
            filter_sql(filter, c, None, None, &opts).is_err(),
            "{name}: filter {filter:?} was accepted"
        );
    }
    // Trusted (admin-defined) rules keep @collection, limited to user collections.
    let trusted = FilterOptions::default();
    assert!(
        filter_sql(
            "@collection.garages.owner = id",
            &fx.builds,
            None,
            None,
            &trusted
        )
        .is_ok()
    );
    assert!(
        filter_sql(
            "@collection._rootUsers.passwordHash ~ 'x'",
            &fx.builds,
            None,
            None,
            &trusted
        )
        .is_err()
    );
}

#[test]
fn untrusted_filter_relation_path_honors_target_list_rule() {
    let fx = untrusted_fixture();
    let load = |n: &str| fx.load(n);
    let opts = FilterOptions {
        param_prefix: "f_",
        untrusted: true,
        load: Some(&load),
        ..Default::default()
    };
    let ids = |filter: &str, auth: Option<Value>| {
        let auth = auth.map(map);
        let f = filter_sql(filter, &fx.builds, auth.as_ref(), None, &opts)
            .unwrap()
            .unwrap();
        list_ids(&fx.db.conn, "builds", Some(&f))
    };
    // Bob probes the VIN of Alice's private garage through her public build.
    assert!(ids("garage.vin = 'VIN-ALICE'", Some(json!({"id": "bob"}))).is_empty());
    // Alice may filter through her own garage.
    assert_eq!(
        ids("garage.vin = 'VIN-ALICE'", Some(json!({"id": "alice"}))),
        ["b1"]
    );
    // Public targets (users has no list rule) are filterable on non-secret columns.
    assert_eq!(ids("author.username = 'bob'", None), ["b2"]);
}

#[test]
fn sort_relation_injection_and_visibility() {
    let fx = untrusted_fixture();
    let load = |n: &str| fx.load(n);
    let trusted = SortOptions {
        untrusted: false,
        load: Some(&load),
    };
    let untrusted = SortOptions {
        untrusted: true,
        load: Some(&load),
    };
    for sort in [
        "author.username) DESC, (SELECT passwordHash FROM _rootUsers",
        "author.username FROM users) --",
        "-author.x;DROP TABLE builds",
        "title DESC; DROP TABLE builds",
    ] {
        for opts in [&trusted, &untrusted] {
            assert!(
                sort_sql(sort, &fx.builds, opts).is_err(),
                "sort {sort:?} accepted (untrusted={})",
                opts.untrusted
            );
        }
    }
    assert!(
        sort_sql("garage.vin", &fx.builds, &untrusted).is_err(),
        "sort through a private target accepted"
    );
    assert!(
        sort_sql("author.email", &fx.builds, &untrusted).is_err(),
        "sort by a secret column accepted"
    );

    let terms = sort_sql("-author.username", &fx.builds, &untrusted).unwrap();
    assert!(
        terms[0].contains("`users` AS _sub"),
        "relation sort is not quoted/aliased: {terms:?}"
    );
    let sql = format!("SELECT id FROM builds ORDER BY {}", terms.join(", "));
    let mut stmt = fx.db.conn.prepare(&sql).unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows, ["b2", "b1"]);
}

#[test]
fn untrusted_filter_nested_rule_through_relations() {
    // Teams have an owner; posts belong to a team (list rule through the
    // relation) and may have a parent post; comments point at posts.
    let teams = collection(
        "teams",
        CollectionKind::Base,
        vec![field("owner", FieldType::Text)],
        "",
    );
    let posts = collection(
        "posts",
        CollectionKind::Base,
        vec![
            field("title", FieldType::Text),
            relation("team", "teams"),
            relation("parent", "posts"),
        ],
        "team.owner = @request.auth.id",
    );
    let comments = collection(
        "comments",
        CollectionKind::Base,
        vec![relation("post", "posts")],
        "",
    );
    let all = [teams, posts.clone(), comments.clone()];
    let load = |n: &str| all.iter().find(|c| c.name == n).cloned();
    let db = GoDb::new();
    db.conn
        .execute_batch(
            "CREATE TABLE teams (id TEXT PRIMARY KEY, owner TEXT);
             CREATE TABLE posts (id TEXT PRIMARY KEY, title TEXT, team TEXT, parent TEXT);
             CREATE TABLE comments (id TEXT PRIMARY KEY, post TEXT);
             INSERT INTO teams VALUES ('t_alice', 'alice'), ('t_bob', 'bob');
             INSERT INTO posts VALUES ('p_bob', 'TOP-SECRET', 't_bob', NULL), ('p_alice', 'mine', 't_alice', 'p_bob');
             INSERT INTO comments VALUES ('c1', 'p_bob'), ('c2', 'p_alice');",
        )
        .unwrap();
    let alice = map(json!({"id": "alice"}));
    let opts = FilterOptions {
        param_prefix: "f_",
        untrusted: true,
        load: Some(&load),
        ..Default::default()
    };
    let rule = filter_sql(
        &posts.rules.list_rule,
        &posts,
        Some(&alice),
        None,
        &FilterOptions {
            param_prefix: "r_",
            load: Some(&load),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();

    // Self-relation: alice's own post points at bob's hidden post; its title must stay hidden.
    let f = filter_sql(
        "parent.title ~ 'TOP-SECRET'",
        &posts,
        Some(&alice),
        None,
        &opts,
    )
    .unwrap()
    .unwrap();
    let combined = moul_rules::SqlFilter {
        sql: format!("({}) AND ({})", rule.sql, f.sql),
        params: f.params.into_iter().chain(rule.params).collect(),
    };
    let got = list_ids(&db.conn, "posts", Some(&combined));
    assert!(
        got.is_empty(),
        "hidden parent title leaked through a self-relation: {got:?}"
    );

    // Cross-collection: the target's rule itself goes through a relation.
    let f = filter_sql("post.title ~ 'mine'", &comments, Some(&alice), None, &opts)
        .unwrap()
        .unwrap();
    assert_eq!(list_ids(&db.conn, "comments", Some(&f)), ["c2"]);
    let f = filter_sql("post.title ~ 'TOP'", &comments, Some(&alice), None, &opts)
        .unwrap()
        .unwrap();
    assert!(
        list_ids(&db.conn, "comments", Some(&f)).is_empty(),
        "hidden post matched through a comment"
    );
}
