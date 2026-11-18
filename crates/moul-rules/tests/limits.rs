//! Input that nests deeply or runs long fails with an error and never
//! exhausts the stack: a stack overflow cannot be caught, and it kills a
//! browser worker or a server. Every case runs on a 256 KiB stack, a
//! quarter of a browser worker's, in whatever build the tests run in.
//!
//! A filter or rule nested past [`MAX_DEPTH`] is refused; the Go engine,
//! whose stacks grow, had no such limit. A rule of more than expr-lang's
//! 10,000 nodes is refused as the Go engine refused it, with its message
//! and position (recorded from it, expr-lang v1.17.8). A long flat chain
//! does not nest, so it runs up to that limit.

use moul_rules::{
    EvalContext, FilterOptions, MAX_DEPTH, Map, SortOptions, check, evaluate, filter_sql, sort_sql,
    translate,
};
use moul_store::schema::{Collection, CollectionKind, Field, FieldType, Relation};
use serde_json::json;

const TOO_DEEP_FILTER: &str = "filter is nested too deeply";

/// Runs `f` on a 256 KiB stack.
fn on_small_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_scoped(s, f)
            .expect("spawn a thread")
            .join()
            .expect("the thread finishes")
    })
}

/// `posts`, with a title and an author in `users`.
fn posts() -> Collection {
    let mut author = Field::new("author", FieldType::Relation);
    author.relation = Some(Relation {
        target_moul: "users".into(),
        cardinality: "1:N".into(),
        on_delete: String::new(),
    });
    let mut c = Collection::new("posts", CollectionKind::Base);
    c.fields = vec![Field::new("title", FieldType::Text), author];
    c
}

fn users(list_rule: &str) -> Collection {
    let mut c = Collection::new("users", CollectionKind::Base);
    c.rules.list_rule = list_rule.into();
    c
}

fn filter_nested(depth: usize, condition: &str) -> String {
    format!("{}{condition}{}", "(".repeat(depth), ")".repeat(depth))
}

#[test]
fn filter_nesting_is_limited() {
    let posts = posts();
    on_small_stack(|| {
        let opts = FilterOptions {
            untrusted: true,
            ..Default::default()
        };
        let run = |filter: &str| filter_sql(filter, &posts, None, None, &opts);
        let deepest = run(&filter_nested(MAX_DEPTH, "title = 'a'"))
            .unwrap()
            .unwrap();
        assert!(deepest.sql.starts_with(&"(".repeat(MAX_DEPTH)));
        for filter in [
            filter_nested(MAX_DEPTH + 1, "title = 'a'"),
            filter_nested(100_000, "title = 'a'"),
            "(".repeat(100_000),
        ] {
            assert_eq!(run(&filter).unwrap_err().message(), TOO_DEEP_FILTER);
        }
        // A long chain does not nest.
        let chain = vec!["title = 'a'"; 2_000].join(" && ");
        assert!(run(&chain).unwrap().is_some());
    });
}

#[test]
fn filter_nesting_counts_the_target_list_rule() {
    // The list rule of `users` nests two more parentheses inside the
    // subquery a client filter on `author.*` gets.
    let users = users("((id != ''))");
    let posts = posts();
    on_small_stack(|| {
        let load = |name: &str| (name == "users").then(|| users.clone());
        let opts = FilterOptions {
            untrusted: true,
            load: Some(&load),
            ..Default::default()
        };
        let run = |depth| {
            let filter = filter_nested(depth, "author.name = 'x'");
            filter_sql(&filter, &posts, None, None, &opts)
        };
        assert!(run(MAX_DEPTH - 2).unwrap().is_some());
        assert_eq!(
            run(MAX_DEPTH - 1).unwrap_err().message(),
            format!("failed to translate users list rule: {TOO_DEEP_FILTER}")
        );
    });
}

#[test]
fn sort_does_not_nest() {
    let posts = posts();
    on_small_stack(|| {
        let opts = SortOptions::default();
        let long = vec!["-title"; 10_000].join(",");
        assert_eq!(sort_sql(&long, &posts, &opts).unwrap().len(), 10_000);
        let nested = filter_nested(100_000, "title");
        let err = sort_sql(&nested, &posts, &opts).unwrap_err();
        assert!(err.message().starts_with("invalid sort field"));
    });
}

/// A rule nested as deep as asked.
type Shape = dyn Fn(usize) -> String;

/// Evaluates `rule` for a record titled `x` with `a` = 1.
fn eval(rule: &str) -> Result<bool, moul_rules::Error> {
    let record: Map = json!({"title": "x", "a": 1})
        .as_object()
        .cloned()
        .unwrap_or_default();
    let ctx = EvalContext {
        auth: None,
        record: &record,
        request: None,
        db: None,
        now_ms: 0,
    };
    evaluate(rule, &ctx)
}

/// Asserts that `evaluate` and `check` refuse `rule` as nested too deeply.
fn assert_too_deep(rule: &str) {
    let want = format!("failed to compile rule '{rule}': expression is nested too deeply (1:");
    for err in [eval(rule).unwrap_err(), check(rule).unwrap_err()] {
        assert!(
            err.message().starts_with(&want),
            "{}",
            &err.message()[..200.min(err.message().len())]
        );
    }
}

#[test]
fn rule_nesting_is_limited() {
    on_small_stack(|| {
        // Each shape at the deepest it may go. Parentheses: the outer
        // expression, one per parenthesis and the right side of `=`.
        let parens = |depth: usize| filter_nested(depth, "title = 'x'");
        // Calls: the outer expression and one per argument list.
        let calls =
            |depth: usize| format!("{}title{} = 'x'", "lower(".repeat(depth), ")".repeat(depth));
        // `&&` and a parenthesis: two per level.
        let groups = |depth: usize| {
            format!(
                "{}title = 'x'{}",
                "a = 1 && (".repeat(depth),
                ")".repeat(depth)
            )
        };
        // A comparison and a parenthesis: two per level. Its type check
        // fails at the top, after the walk down: a bool is no number.
        let comparisons = |depth: usize| format!("{}1{}", "0 < (".repeat(depth), ")".repeat(depth));
        // A method call: one per argument list. It fails when it runs, on
        // the nil callee at the bottom.
        let methods = |depth: usize| {
            format!(
                "{}title{} = 'x'",
                "@request.body.f(".repeat(depth),
                ")".repeat(depth)
            )
        };
        // Each shape, its deepest rule and what `evaluate` makes of it: true,
        // or an error saying this.
        let shapes: [(&Shape, usize, &str); 5] = [
            (&parens, MAX_DEPTH - 2, ""),
            (&calls, MAX_DEPTH - 1, ""),
            (&groups, (MAX_DEPTH - 2) / 2, ""),
            (
                &comparisons,
                (MAX_DEPTH - 1) / 2,
                "mismatched types int and bool",
            ),
            (&methods, MAX_DEPTH - 1, "cannot call nil"),
        ];
        for (shape, deepest, error) in shapes {
            let rule = shape(deepest);
            assert_eq!(check(&rule), Ok(()), "{rule}");
            match eval(&rule) {
                Ok(allowed) => assert!(allowed && error.is_empty(), "{rule}"),
                Err(e) => assert!(
                    !error.is_empty() && e.message().contains(error),
                    "{rule}: {}",
                    e.message()
                ),
            }
            assert_too_deep(&shape(deepest + 1));
        }
        assert_too_deep(&parens(100_000));
        assert_too_deep(&calls(100_000));
    });
}

/// Asserts that `evaluate` and `check` refuse `rule` with expr-lang's
/// message for more than 10,000 nodes, at `column`, as the Go engine did.
fn assert_too_many_nodes(rule: &str, column: usize) {
    let src = translate(rule).unwrap();
    let want = format!(
        "failed to compile rule '{rule}': compilation failed: expression exceeds maximum allowed nodes (1:{column})\n | {src}\n | {}^",
        ".".repeat(column - 1)
    );
    assert_eq!(eval(rule).unwrap_err().message(), want);
    assert_eq!(check(rule).unwrap_err().message(), want);
}

#[test]
fn rule_node_limit_matches_go() {
    on_small_stack(|| {
        // `a == 1` is three nodes and `&&` one: 2,500 conditions make 9,999.
        let conditions = |n: usize| vec!["a = 1"; n].join(" && ");
        assert_eq!(eval(&conditions(2_500)), Ok(true));
        assert_eq!(check(&conditions(2_500)), Ok(()));
        assert_too_many_nodes(&conditions(2_501), 25_006);
        // A bare name is one node.
        let names = |n: usize| vec!["a"; n].join(" && ");
        assert_too_many_nodes(&names(5_001), 25_001);
        assert_too_many_nodes(&names(5_002), 25_003);
        // `title:lower = 'x'` is six nodes with its `&&`.
        let lowered = |n: usize| vec!["title:lower = 'x'"; n].join(" && ");
        assert_eq!(eval(&lowered(1_666)), Ok(true));
        assert_too_many_nodes(&lowered(1_667), 38_337);
        // A member is two nodes: `request.body` and 4,998 more make 9,999.
        let path = |n: usize| format!("@request.body{}", ".a".repeat(n));
        assert_eq!(check(&path(4_998)), Ok(()));
        assert_too_many_nodes(&path(4_999), 10_010);
        // Parentheses make no node.
        let grouped = |n: usize| format!("({})", vec!["(a = 1)"; n].join(" && "));
        assert_eq!(eval(&grouped(2_500)), Ok(true));
    });
}

#[test]
fn long_chains_run_on_a_small_stack() {
    on_small_stack(|| {
        // `||` runs every condition before the last one holds.
        let mut any = vec!["a = 2"; 2_400];
        any.push("a = 1");
        assert_eq!(eval(&any.join(" || ")), Ok(true));
        // Folding drops each `true &&`.
        let folded = format!("{}title = 'x'", "true && ".repeat(4_000));
        assert_eq!(eval(&folded), Ok(true));
        // A comparison copies its middle operand, here a long path.
        let between = format!("0 < @request.body{} < 2", ".a".repeat(3_000));
        assert_eq!(check(&between), Ok(()));
        assert!(eval(&between).is_err());
    });
}
