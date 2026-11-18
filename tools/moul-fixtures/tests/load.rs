//! Every fixture in `fixtures/go` parses, and every database state it names exists.

use std::collections::BTreeSet;

use moul_fixtures::{Fixtures, RuleKind};

#[test]
fn manifest_matches_the_files() {
    let fx = Fixtures::default();
    let manifest = fx.manifest().expect("manifest");
    assert_eq!(manifest.commit.len(), 40, "commit should be a full sha");
    assert!(
        !manifest.dirty,
        "fixtures must come from a clean Go checkout"
    );

    let http = fx.http_paths().expect("http fixtures");
    assert_eq!(http.len() as u64, manifest.counts["http_tests"]);

    for kind in RuleKind::ALL {
        let key = match kind {
            RuleKind::Evaluate => "rules_evaluate",
            RuleKind::FilterSql => "rules_filter_sql",
            RuleKind::SortSql => "rules_sort_sql",
        };
        let cases = fx.rules(kind).expect("rule cases");
        assert_eq!(cases.len() as u64, manifest.counts[key], "{key}");
    }
}

#[test]
fn every_fixture_parses_and_its_databases_exist() {
    let fx = Fixtures::default();
    let mut db_ids = BTreeSet::new();
    let mut exchanges = 0;

    for path in fx.http_paths().expect("http fixtures") {
        let f = fx.http(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(!f.sessions.is_empty(), "{}: no sessions", path.display());
        for s in &f.sessions {
            exchanges += s.exchanges.len();
            if let Some(id) = &s.db_before {
                db_ids.insert(id.clone());
            }
            for ex in &s.exchanges {
                assert!(
                    (100..600).contains(&ex.response.status),
                    "{}",
                    path.display()
                );
            }
        }
    }
    assert!(exchanges > 0);

    for kind in RuleKind::ALL {
        for case in fx.rules(kind).expect("rule cases") {
            assert!(!case.sources.is_empty(), "{}: no sources", case.id);
            if let Some(id) = case.input.get("db").and_then(|v| v.as_str()) {
                db_ids.insert(id.to_owned());
            }
        }
    }

    for id in db_ids {
        let db = fx.db(&id).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            db.schema.iter().any(|o| o.kind == "table"),
            "db {id} has no tables"
        );
    }
}
