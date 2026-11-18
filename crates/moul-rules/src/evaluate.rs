//! Go's `rules.EvaluateRule`: does a rule allow this record for this caller.
//!
//! A rule is translated to an expr-lang expression (`crate::translate`),
//! parsed (`crate::expr`), type-checked against the values at hand and run,
//! following expr-lang's rules: a mismatch it finds while checking is a
//! compile error, one it meets while running is a run error.

use std::collections::BTreeMap;
use std::rc::Rc;

use moul_store::schema::{Collection, FieldType};
use serde_json::Value;

use crate::expr::{ExprError, Node, NodeKind, parse};
use crate::translate::{Group, is_date_macro, translate};
use crate::{Error, Map, go};

/// What the evaluator needs from the database: collection definitions and
/// single-row reads. moul-core implements it over its connection.
///
/// Only `@collection` conditions and the `get(`, `exists(` and `strftime(`
/// functions use it; without one they see no rows, as in the Go engine.
pub trait Lookup {
    /// The collection named `name`, or `None` if there is none.
    fn collection(&self, name: &str) -> Option<Collection>;

    /// Runs one read-only `SELECT` and returns its first row, or `None` when
    /// there is none.
    ///
    /// Placeholders are numbered, `?1`, `?2`, ..., and `params` holds their
    /// values in that order: null, a boolean (bind as 0 or 1), a number or a
    /// string. Columns come back by name: `NULL` as null, `INTEGER` and
    /// `REAL` as numbers, `TEXT` as strings and a `BLOB` as a string of its
    /// bytes. An error (a bad table or column, say) is returned as text; the
    /// evaluator treats it as no row.
    fn first_row(&self, sql: &str, params: &[Value]) -> Result<Option<Map>, String>;
}

/// Everything a rule can read.
#[derive(Clone, Copy)]
pub struct EvalContext<'a> {
    /// The signed-in caller, `@request.auth.*`. `None` reads as an empty
    /// `id`, `username` and `email`.
    pub auth: Option<&'a Map>,
    /// The record the rule is about. Its fields are the rule's bare names.
    pub record: &'a Map,
    /// The request, `@request.*`: `method`, `query`, `headers` and `body`.
    /// Missing parts read as empty. The Go engine gave the first value of
    /// each query parameter and header, with header names lowercase and `-`
    /// written `_`.
    pub request: Option<&'a Map>,
    /// The database, for `@collection`, `get(`, `exists(` and `strftime(`.
    pub db: Option<&'a dyn Lookup>,
    /// The current time in milliseconds since the Unix epoch, for `@now`,
    /// `@todayStart` and the other date macros. The caller supplies it so
    /// the crate needs no clock.
    pub now_ms: i64,
}

/// Go's `EvaluateRule`. An empty rule allows everything.
pub fn evaluate(rule: &str, ctx: &EvalContext<'_>) -> Result<bool, Error> {
    if rule.is_empty() {
        return Ok(true);
    }
    let (src, groups) =
        translate(rule).map_err(|e| Error::new(format!("failed to translate rule: {e}")))?;
    let mut env = Env::new(ctx);
    for group in &groups {
        let found = env.run_group(group);
        env.groups.push((group.placeholder(), found));
    }
    let compile = |e: ExprError| {
        Error::new(format!(
            "failed to compile rule '{rule}': {}",
            e.render(&src)
        ))
    };
    let execute = |e: ExprError| {
        Error::new(format!(
            "failed to execute rule '{rule}': {}",
            e.render(&src)
        ))
    };
    let mut ast = parse(&src).map_err(compile)?;
    let nature = env.check(&ast).map_err(compile)?;
    if !matches!(nature, Nature::Unknown | Nature::Bool) {
        return Err(compile(ExprError::plain(format!(
            "expected bool, but got {}",
            nature.name()
        ))));
    }
    // expr-lang folds constants after checking, which decides what can
    // still fail at run time: `x && true` is just `x`.
    for _ in 0..1000 {
        if !fold(&mut ast) {
            break;
        }
    }
    // The result is cast to bool: nil is false, anything else not a bool is
    // an error at the start of the expression.
    match env.run(&ast).map_err(execute)? {
        Val::Bool(b) => Ok(b),
        Val::Nil => Ok(false),
        other => Err(execute(ExprError::at(
            0,
            format!("invalid operation: bool({})", other.type_name()),
        ))),
    }
}

/// One pass of expr-lang's constant folding, children first. A folded node
/// keeps the position of the node it replaces. Returns whether anything
/// changed.
///
/// First children are taken out in a loop and put back from the bottom
/// up, so a long chain needs no deep recursion (see [`Node`]).
fn fold(root: &mut Node) -> bool {
    let mut parents = Vec::new();
    let mut node = std::mem::replace(root, Node::placeholder());
    while let Some(first) = node.first_mut() {
        let first = std::mem::replace(first, Node::placeholder());
        parents.push(node);
        node = first;
    }
    // `node` is a leaf now, and a leaf never folds.
    let mut applied = false;
    while let Some(mut parent) = parents.pop() {
        if let Some(first) = parent.first_mut() {
            *first = node;
        }
        applied |= fold_parent(&mut parent);
        node = parent;
    }
    *root = node;
    applied
}

/// Folds the children of `node` other than its first, which is folded
/// already, then `node` itself.
fn fold_parent(node: &mut Node) -> bool {
    let applied = match &mut node.kind {
        NodeKind::Binary { right, .. } => fold(right),
        NodeKind::Call { args, .. } => {
            let mut applied = false;
            for arg in args {
                applied |= fold(arg);
            }
            applied
        }
        NodeKind::Cond { yes, no, .. } => fold(yes) | fold(no),
        _ => false,
    };
    fold_node(node) | applied
}

/// Folds `node` itself, its children being folded. Returns whether it
/// changed.
fn fold_node(node: &mut Node) -> bool {
    let replacement = match &node.kind {
        NodeKind::Unary { op, node: inner } => match (*op, &inner.kind) {
            ("-", NodeKind::Int(i)) => Some(NodeKind::Int(i.wrapping_neg())),
            ("-", NodeKind::Float(f)) => Some(NodeKind::Float(-f)),
            ("+", NodeKind::Int(i)) => Some(NodeKind::Int(*i)),
            ("+", NodeKind::Float(f)) => Some(NodeKind::Float(*f)),
            ("!" | "not", NodeKind::Bool(b)) => Some(NodeKind::Bool(!b)),
            _ => None,
        },
        NodeKind::Binary { op, left, right } => {
            use NodeKind::{Bool, Float, Int, Str};
            match (*op, &left.kind, &right.kind) {
                ("+", Int(a), Int(b)) => Some(Int(a.wrapping_add(*b))),
                ("-", Int(a), Int(b)) => Some(Int(a.wrapping_sub(*b))),
                ("+" | "-", Int(_) | Float(_), Int(_) | Float(_)) => {
                    let n = |k: &NodeKind| match k {
                        Int(i) => *i as f64,
                        Float(f) => *f,
                        _ => 0.0,
                    };
                    let (a, b) = (n(&left.kind), n(&right.kind));
                    Some(Float(if *op == "+" { a + b } else { a - b }))
                }
                ("+", Str(a), Str(b)) => Some(Str(format!("{a}{b}"))),
                ("&&" | "and", Bool(true), _) => Some(right.kind.clone()),
                ("&&" | "and", _, Bool(true)) => Some(left.kind.clone()),
                ("&&" | "and", Bool(false), _) | ("&&" | "and", _, Bool(false)) => {
                    Some(Bool(false))
                }
                ("||" | "or", Bool(false), _) => Some(right.kind.clone()),
                ("||" | "or", _, Bool(false)) => Some(left.kind.clone()),
                ("||" | "or", Bool(true), _) | ("||" | "or", _, Bool(true)) => Some(Bool(true)),
                ("==", Int(a), Int(b)) => Some(Bool(a == b)),
                ("==", Str(a), Str(b)) => Some(Bool(a == b)),
                ("==", Bool(a), Bool(b)) => Some(Bool(a == b)),
                _ => None,
            }
        }
        _ => None,
    };
    match replacement {
        Some(kind) => {
            node.kind = kind;
            true
        }
        None => false,
    }
}

/// Checks a rule's syntax without evaluating it: the errors `evaluate`
/// reports whatever the record.
pub fn check(rule: &str) -> Result<(), Error> {
    if rule.is_empty() {
        return Ok(());
    }
    let (src, _) =
        translate(rule).map_err(|e| Error::new(format!("failed to translate rule: {e}")))?;
    parse(&src).map_err(|e| {
        Error::new(format!(
            "failed to compile rule '{rule}': {}",
            e.render(&src)
        ))
    })?;
    Ok(())
}

// ---------------------------------------------------------------- values

/// A value while a rule runs, typed as the Go engine's would be: JSON
/// numbers are `float64`, number literals in a rule are `int`.
#[derive(Clone, Debug)]
enum Val {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Rc<Vec<Val>>),
    Map(Rc<BTreeMap<String, Val>>),
    Func(Builtin),
}

impl Val {
    fn from_json(v: &Value) -> Val {
        match v {
            Value::Null => Val::Nil,
            Value::Bool(b) => Val::Bool(*b),
            Value::Number(n) => Val::Float(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => Val::Str(s.clone()),
            Value::Array(items) => Val::List(Rc::new(items.iter().map(Val::from_json).collect())),
            Value::Object(map) => Val::from_map(map),
        }
    }

    fn from_map(map: &Map) -> Val {
        Val::Map(Rc::new(
            map.iter()
                .map(|(k, v)| (k.clone(), Val::from_json(v)))
                .collect(),
        ))
    }

    /// Go's `%T`.
    fn type_name(&self) -> &'static str {
        match self {
            Val::Nil => "<nil>",
            Val::Bool(_) => "bool",
            Val::Int(_) => "int",
            Val::Float(_) => "float64",
            Val::Str(_) => "string",
            Val::List(_) => "[]interface {}",
            Val::Map(_) => "map[string]interface {}",
            Val::Func(b) => b.sig().go,
        }
    }

    /// The type as a failed Go type assertion names it.
    fn assert_name(&self) -> &'static str {
        match self {
            Val::Nil => "nil",
            other => other.type_name(),
        }
    }

    /// Go's `%v`.
    fn format(&self) -> String {
        match self {
            Val::Nil => "<nil>".into(),
            Val::Bool(b) => b.to_string(),
            Val::Int(i) => i.to_string(),
            Val::Float(f) => go::format_float(*f),
            Val::Str(s) => s.clone(),
            Val::List(items) => {
                let parts: Vec<String> = items.iter().map(Val::format).collect();
                format!("[{}]", parts.join(" "))
            }
            Val::Map(map) => {
                let parts: Vec<String> = map
                    .iter()
                    .map(|(k, v)| format!("{k}:{}", v.format()))
                    .collect();
                format!("map[{}]", parts.join(" "))
            }
            Val::Func(_) => "0x0".into(),
        }
    }

    /// A value to bind to a SQL placeholder. Lists, maps and functions
    /// cannot be bound, as in Go's `database/sql`.
    fn to_param(&self) -> Option<Value> {
        Some(match self {
            Val::Nil => Value::Null,
            Val::Bool(b) => Value::Bool(*b),
            Val::Int(i) => Value::from(*i),
            Val::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Val::Str(s) => Value::String(s.clone()),
            Val::List(_) | Val::Map(_) | Val::Func(_) => return None,
        })
    }

    /// The Go helper `toFloat64`.
    fn to_f64(&self) -> Option<f64> {
        match self {
            Val::Float(f) => Some(*f),
            Val::Int(i) => Some(*i as f64),
            Val::Str(s) => go::parse_float(s),
            _ => None,
        }
    }

    /// The Go helper `toSlice`.
    fn to_slice(&self) -> Vec<Val> {
        match self {
            Val::Nil => Vec::new(),
            Val::List(items) => items.to_vec(),
            other => vec![other.clone()],
        }
    }
}

/// expr-lang's `runtime.Equal`.
fn equal(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Int(x), Val::Int(y)) => x == y,
        (Val::Int(x), Val::Float(y)) | (Val::Float(y), Val::Int(x)) => *x as f64 == *y,
        (Val::Float(x), Val::Float(y)) => x == y,
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Nil, Val::Nil) => true,
        (Val::List(x), Val::List(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| equal(a, b))
        }
        (Val::Map(x), Val::Map(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| equal(v, w)))
        }
        _ => false,
    }
}

/// The Go helper `equalValues`: numbers by value, anything else by `%v`.
fn equal_values(a: &Val, b: &Val) -> bool {
    if matches!(a, Val::Nil) || matches!(b, Val::Nil) {
        return matches!((a, b), (Val::Nil, Val::Nil));
    }
    if let (Some(x), Some(y)) = (a.to_f64(), b.to_f64()) {
        return x == y;
    }
    a.format() == b.format()
}

/// The Go helper `likeMatch`: `%` is any run of characters, `_` one
/// character, neither crossing a newline; case is ignored. A pattern
/// without `%` matches anywhere in the text.
fn like(a: &Val, b: &Val) -> bool {
    let (Val::Str(text), Val::Str(pattern)) = (a, b) else {
        return false;
    };
    if pattern.contains('%') {
        like_match(text, pattern)
    } else {
        like_match(text, &format!("%{pattern}%"))
    }
}

fn like_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    // row[j]: does t[..i] match p[..j].
    let mut row = vec![false; p.len() + 1];
    row[0] = true;
    for j in 1..=p.len() {
        row[j] = row[j - 1] && p[j - 1] == '%';
    }
    for &c in &t {
        let mut diagonal = row[0];
        row[0] = false;
        for j in 1..=p.len() {
            let above = row[j];
            row[j] = match p[j - 1] {
                '%' => row[j - 1] || (above && c != '\n'),
                '_' => diagonal && c != '\n',
                pc => diagonal && fold_eq(pc, c),
            };
            diagonal = above;
        }
    }
    row[p.len()]
}

fn fold_eq(a: char, b: char) -> bool {
    fn single(mut it: impl Iterator<Item = char>) -> Option<char> {
        let c = it.next()?;
        it.next().is_none().then_some(c)
    }
    a == b
        || single(a.to_lowercase()) == single(b.to_lowercase())
            && single(a.to_lowercase()).is_some()
        || single(a.to_uppercase()) == single(b.to_uppercase())
            && single(a.to_uppercase()).is_some()
}

// ---------------------------------------------------------------- functions

/// The functions a translated rule can call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Builtin {
    Lower,
    Length,
    Isset,
    Changed,
    GeoDistance,
    Strftime,
    Like,
    Any(Cmp),
    All(Cmp),
    Exists,
    Get,
}

/// The comparison of an `any_*` or `all_*` function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp {
    Eq,
    Neq,
    Like,
    NotLike,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// A parameter type: anything, or a Go `string`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Param {
    Any,
    Str,
}

struct Sig {
    params: &'static [Param],
    variadic: bool,
    out: Nature,
    /// The Go type, for messages.
    go: &'static str,
}

const TWO_ANY: Sig = Sig {
    params: &[Param::Any, Param::Any],
    variadic: false,
    out: Nature::Bool,
    go: "func(interface {}, interface {}) bool",
};

impl Builtin {
    fn by_name(name: &str) -> Option<Builtin> {
        let cmp = |s: &str| {
            Some(match s {
                "eq" => Cmp::Eq,
                "neq" => Cmp::Neq,
                "like" => Cmp::Like,
                "not_like" => Cmp::NotLike,
                "gt" => Cmp::Gt,
                "gte" => Cmp::Gte,
                "lt" => Cmp::Lt,
                "lte" => Cmp::Lte,
                _ => return None,
            })
        };
        Some(match name {
            "lower" => Builtin::Lower,
            "length" => Builtin::Length,
            "isset" => Builtin::Isset,
            "changed" => Builtin::Changed,
            "geoDistance" => Builtin::GeoDistance,
            "strftime" => Builtin::Strftime,
            "like" => Builtin::Like,
            "exists" => Builtin::Exists,
            "get" => Builtin::Get,
            _ => match name.strip_prefix("any_") {
                Some(c) => Builtin::Any(cmp(c)?),
                None => Builtin::All(cmp(name.strip_prefix("all_")?)?),
            },
        })
    }

    fn sig(self) -> &'static Sig {
        use Param::{Any, Str};
        match self {
            Builtin::Lower => &Sig {
                params: &[Any],
                variadic: false,
                out: Nature::Str,
                go: "func(interface {}) string",
            },
            Builtin::Length => &Sig {
                params: &[Any],
                variadic: false,
                out: Nature::Int,
                go: "func(interface {}) int",
            },
            Builtin::Isset => &Sig {
                params: &[Any, Str],
                variadic: false,
                out: Nature::Bool,
                go: "func(interface {}, string) bool",
            },
            Builtin::Changed => &Sig {
                params: &[Any, Str, Any],
                variadic: false,
                out: Nature::Bool,
                go: "func(interface {}, string, interface {}) bool",
            },
            Builtin::GeoDistance => &Sig {
                params: &[Any, Any, Any, Any],
                variadic: false,
                out: Nature::Float,
                go: "func(interface {}, interface {}, interface {}, interface {}) float64",
            },
            Builtin::Strftime => &Sig {
                params: &[Any, Any],
                variadic: true,
                out: Nature::Str,
                go: "func(interface {}, ...interface {}) string",
            },
            Builtin::Like | Builtin::Any(_) | Builtin::All(_) => &TWO_ANY,
            Builtin::Exists => &Sig {
                params: &[Str, Str, Any],
                variadic: true,
                out: Nature::Bool,
                go: "func(string, string, ...interface {}) bool",
            },
            Builtin::Get => &Sig {
                params: &[Str, Any],
                variadic: false,
                out: Nature::Unknown,
                go: "func(string, interface {}) interface {}",
            },
        }
    }
}

// ---------------------------------------------------------------- types

/// What the checker knows about a value: expr-lang's `Nature`, cut down to
/// the types a rule can meet. `Unknown` is anything (`interface{}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nature {
    Unknown,
    Nil,
    Bool,
    Int,
    Float,
    Str,
    List,
    Map,
    Func(Builtin),
}

impl Nature {
    fn of_json(v: &Value) -> Nature {
        match v {
            Value::Null => Nature::Nil,
            Value::Bool(_) => Nature::Bool,
            Value::Number(_) => Nature::Float,
            Value::String(_) => Nature::Str,
            Value::Array(_) => Nature::List,
            Value::Object(_) => Nature::Map,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Nature::Unknown | Nature::Nil => "unknown",
            Nature::Bool => "bool",
            Nature::Int => "int",
            Nature::Float => "float64",
            Nature::Str => "string",
            Nature::List => "[]interface {}",
            Nature::Map => "map[string]interface {}",
            Nature::Func(b) => b.sig().go,
        }
    }

    fn unknown(self) -> bool {
        self == Nature::Unknown
    }

    fn number(self) -> bool {
        matches!(self, Nature::Int | Nature::Float)
    }

    fn assignable_to(self, other: Nature) -> bool {
        match (self, other) {
            (Nature::Func(a), Nature::Func(b)) => a.sig().go == b.sig().go,
            (a, b) => a == b,
        }
    }

    fn comparable_to(self, other: Nature) -> bool {
        self.unknown()
            || other.unknown()
            || self == Nature::Nil
            || other == Nature::Nil
            || (self.number() && other.number())
            || (self == Nature::List && other == Nature::List)
            || self.assignable_to(other)
    }

    /// expr-lang's `MaybeCompatible`: one side unknown and the other passes
    /// `check`, or both unknown.
    fn maybe(self, other: Nature, check: impl Fn(Nature) -> bool) -> bool {
        (self.unknown() && other.unknown())
            || (self.unknown() && check(other))
            || (other.unknown() && check(self))
    }
}

fn mismatched(op: &str, l: Nature, r: Nature, at: usize) -> ExprError {
    ExprError::at(
        at,
        format!(
            "invalid operation: {op} (mismatched types {} and {})",
            l.name(),
            r.name()
        ),
    )
}

// ---------------------------------------------------------------- environment

struct Env<'a> {
    ctx: &'a EvalContext<'a>,
    /// The record, the caller and the request, read once.
    record: Val,
    auth: Val,
    request: Val,
    /// `@collection` group placeholders and whether a row matched.
    groups: Vec<(String, bool)>,
}

impl<'a> Env<'a> {
    fn new(ctx: &'a EvalContext<'a>) -> Env<'a> {
        let auth = match ctx.auth {
            Some(a) => Val::from_map(a),
            None => {
                let empty = |k: &str| (k.to_owned(), Val::Str(String::new()));
                Val::Map(Rc::new(
                    ["id", "username", "email"].into_iter().map(empty).collect(),
                ))
            }
        };
        let mut request = BTreeMap::new();
        request.insert("auth".to_owned(), auth.clone());
        for key in ["body", "headers", "query"] {
            request.insert(key.to_owned(), Val::Map(Rc::default()));
        }
        request.insert("method".to_owned(), Val::Str(String::new()));
        for (k, v) in ctx.request.into_iter().flatten() {
            request.insert(k.clone(), Val::from_json(v));
        }
        Env {
            ctx,
            record: Val::from_map(ctx.record),
            auth,
            request: Val::Map(Rc::new(request)),
            groups: Vec::new(),
        }
    }

    /// A top-level name, as the Go engine's environment map resolves it;
    /// `None` when it is not defined.
    fn get(&self, name: &str) -> Option<Val> {
        if let Some((_, found)) = self.groups.iter().find(|(k, _)| k == name) {
            return Some(Val::Bool(*found));
        }
        if let Some(b) = Builtin::by_name(name) {
            return Some(Val::Func(b));
        }
        if let Some(m) = name.strip_prefix('_')
            && is_date_macro(m)
        {
            return go::date_macro(m, self.ctx.now_ms).map(Val::Str);
        }
        match name {
            "request" => Some(self.request.clone()),
            "auth" => Some(self.auth.clone()),
            "_record" => Some(self.record.clone()),
            _ => match &self.record {
                Val::Map(fields) => fields.get(name).cloned(),
                _ => None,
            },
        }
    }

    fn nature(&self, name: &str) -> Nature {
        if self.groups.iter().any(|(k, _)| k == name) {
            return Nature::Bool;
        }
        if let Some(b) = Builtin::by_name(name) {
            return Nature::Func(b);
        }
        if name.strip_prefix('_').is_some_and(is_date_macro) {
            return Nature::Str;
        }
        match name {
            "request" | "auth" | "_record" => Nature::Map,
            _ => self
                .ctx
                .record
                .get(name)
                .map_or(Nature::Unknown, Nature::of_json),
        }
    }

    /// The Go helper `resolvePath` over the environment.
    fn resolve_path(&self, path: &str) -> Val {
        let mut parts = path.split('.');
        let first = parts.next().unwrap_or_default();
        let mut current = self.get(first).unwrap_or(Val::Nil);
        for part in parts {
            current = match current {
                Val::Nil => return Val::Nil,
                Val::List(items) => {
                    let found = items
                        .iter()
                        .map(|i| field(i, part))
                        .filter(|v| !matches!(v, Val::Nil));
                    Val::List(Rc::new(found.collect()))
                }
                other => field(&other, part),
            };
        }
        current
    }

    /// Whether a row of the group's table meets all its conditions.
    fn run_group(&self, group: &Group) -> bool {
        let Some(db) = self.ctx.db else { return false };
        if go::validate_table_name(&group.table).is_err() {
            return false;
        }
        let mut terms = Vec::new();
        let mut params = Vec::new();
        for (i, c) in group.conditions.iter().enumerate() {
            let op = c.operator.strip_prefix('?').unwrap_or(&c.operator);
            let op = match op {
                "=" | "==" => "=",
                "~" => "LIKE",
                "!~" => "NOT LIKE",
                other => other,
            };
            let mut value = if c.is_path {
                self.resolve_path(&c.value)
            } else {
                Val::Str(c.value.clone())
            };
            if (op == "LIKE" || op == "NOT LIKE")
                && let Val::Str(s) = &value
                && !s.contains('%')
            {
                value = Val::Str(format!("%{s}%"));
            }
            let Some(param) = value.to_param() else {
                return false;
            };
            terms.push(format!(
                "{} {op} ?{}",
                moul_store::schema::quote(&c.field),
                i + 1
            ));
            params.push(param);
        }
        if terms.is_empty() {
            return false;
        }
        let sql = format!(
            "SELECT 1 FROM {} WHERE {} LIMIT 1",
            moul_store::schema::quote(&group.table),
            terms.join(" AND ")
        );
        matches!(db.first_row(&sql, &params), Ok(Some(_)))
    }

    // ------------------------------------------------------------ checker

    /// expr-lang's checker: the type of `node`, or the first type error.
    ///
    /// First children are checked first, so the walk follows them in a
    /// loop: a long chain needs no deep recursion (see [`Node`]).
    fn check(&self, node: &Node) -> Result<Nature, ExprError> {
        let (parents, leaf) = node.spine();
        let mut nature = match &leaf.kind {
            NodeKind::Bool(_) => Nature::Bool,
            NodeKind::Int(_) => Nature::Int,
            NodeKind::Float(_) => Nature::Float,
            NodeKind::Str(_) => Nature::Str,
            NodeKind::Ident(name) => self.nature(name),
            // `Nil`: a node with children is never at the bottom.
            _ => Nature::Nil,
        };
        for parent in parents.into_iter().rev() {
            nature = self.check_parent(parent, nature)?;
        }
        Ok(nature)
    }

    /// The type of `node`, whose first child has the type `first`. Each
    /// kind has a function of its own, so the frames of a deep check stay
    /// small (see `tri!`).
    fn check_parent(&self, node: &Node, first: Nature) -> Result<Nature, ExprError> {
        match &node.kind {
            NodeKind::Member { prop, method, .. } => check_member(first, prop, *method, node.at),
            NodeKind::Unary { op, .. } => check_unary(op, first, node.at),
            NodeKind::Binary { op, right, .. } => {
                let r = tri!(self.check(right));
                self.check_binary(op, first, r, node.at)
            }
            NodeKind::Cond { cond, yes, no } => self.check_cond(first, cond.at, yes, no),
            NodeKind::Call { callee, args } => self.check_call(first, callee, args, node.at),
            // A leaf has no first child, so it is never a parent.
            _ => Ok(first),
        }
    }

    /// The type of `cond ? yes : no`, the condition having the type `c`.
    fn check_cond(
        &self,
        c: Nature,
        cond_at: usize,
        yes: &Node,
        no: &Node,
    ) -> Result<Nature, ExprError> {
        if c != Nature::Bool && !c.unknown() {
            return Err(ExprError::at(
                cond_at,
                format!("non-bool expression (type {}) used as condition", c.name()),
            ));
        }
        let a = tri!(self.check(yes));
        let b = tri!(self.check(no));
        Ok(match (a, b) {
            (Nature::Nil, b) => b,
            (a, Nature::Nil) => a,
            (a, b) if a.assignable_to(b) => a,
            _ => Nature::Unknown,
        })
    }

    fn check_binary(&self, op: &str, l: Nature, r: Nature, at: usize) -> Result<Nature, ExprError> {
        let ok = match op {
            "==" | "!=" => l.comparable_to(r).then_some(Nature::Bool),
            "||" | "or" | "&&" | "and" => ((l == Nature::Bool && r == Nature::Bool)
                || l.maybe(r, |n| n == Nature::Bool))
            .then_some(Nature::Bool),
            "<" | ">" | "<=" | ">=" => ((l.number() && r.number())
                || (l == Nature::Str && r == Nature::Str)
                || l.maybe(r, |n| n.number() || n == Nature::Str))
            .then_some(Nature::Bool),
            "-" | "+" => {
                if l.number() && r.number() {
                    Some(if l == Nature::Int && r == Nature::Int {
                        Nature::Int
                    } else {
                        Nature::Float
                    })
                } else if op == "+" && l == Nature::Str && r == Nature::Str {
                    Some(Nature::Str)
                } else if l.maybe(r, |n| n.number() || (op == "+" && n == Nature::Str)) {
                    Some(Nature::Unknown)
                } else {
                    None
                }
            }
            "in" => {
                if r == Nature::Map {
                    if !l.unknown() && l != Nature::Str {
                        return Err(ExprError::at(
                            at,
                            format!("cannot use {} as type string in map key", l.name()),
                        ));
                    }
                    Some(Nature::Bool)
                } else if r == Nature::List
                    || (l.unknown() && matches!(r, Nature::Str | Nature::List | Nature::Map))
                    || r.unknown()
                {
                    Some(Nature::Bool)
                } else {
                    None
                }
            }
            "contains" | "startsWith" | "endsWith" => ((l == Nature::Str && r == Nature::Str)
                || l.maybe(r, |n| n == Nature::Str))
            .then_some(Nature::Bool),
            "??" => Some(match (l, r) {
                (Nature::Nil, r) => r,
                (l, Nature::Nil) => l,
                (l, r) if r.assignable_to(l) => l,
                _ => Nature::Unknown,
            }),
            _ => {
                return Err(ExprError::at(
                    at,
                    format!("operator {op} is not supported in rules"),
                ));
            }
        };
        ok.ok_or_else(|| mismatched(op, l, r, at))
    }

    /// The type of a call whose callee has the type `callee_type`, with
    /// expr-lang's `checkArguments` for a known function.
    fn check_call(
        &self,
        callee_type: Nature,
        callee: &Node,
        args: &[Node],
        at: usize,
    ) -> Result<Nature, ExprError> {
        let name = match &callee.kind {
            NodeKind::Ident(name) => name.as_str(),
            NodeKind::Member { prop, .. } => prop.as_str(),
            _ => "function",
        };
        let sig = match callee_type {
            Nature::Unknown => return Ok(Nature::Unknown),
            Nature::Func(b) => b.sig(),
            other => return Err(not_callable(name, other, at)),
        };
        if let Some(message) = argument_count_error(name, sig, args.len()) {
            for arg in args {
                tri!(self.check(arg));
            }
            return Err(ExprError::at(at, message));
        }
        for (i, arg) in args.iter().enumerate() {
            let a = tri!(self.check(arg));
            tri!(check_argument(name, sig, i, a, arg.at));
        }
        Ok(sig.out)
    }

    // ------------------------------------------------------------ runtime

    /// Runs `node`. First children run first, except that a call runs its
    /// arguments before its callee, so the walk follows first children in
    /// a loop, running a call's arguments on the way down: a long chain
    /// needs no deep recursion (see [`Node`]).
    fn run(&self, node: &Node) -> Result<Val, ExprError> {
        let mut parents = Vec::new();
        let mut node = node;
        loop {
            let mut args = Vec::new();
            if let NodeKind::Call { args: nodes, .. } = &node.kind {
                for arg in nodes {
                    args.push(tri!(self.run(arg)));
                }
            }
            let Some(first) = node.first() else { break };
            parents.push((node, args));
            node = first;
        }
        let mut val = match &node.kind {
            NodeKind::Bool(b) => Val::Bool(*b),
            NodeKind::Int(i) => Val::Int(*i),
            NodeKind::Float(f) => Val::Float(*f),
            NodeKind::Str(s) => Val::Str(s.clone()),
            NodeKind::Ident(name) => self.get(name).unwrap_or(Val::Nil),
            // `Nil`: a node with children is never at the bottom.
            _ => Val::Nil,
        };
        for (parent, args) in parents.into_iter().rev() {
            val = tri!(self.run_parent(parent, val, args));
        }
        Ok(val)
    }

    /// The value of `node`, whose first child ran to `first`. A call's
    /// arguments ran to `args`.
    fn run_parent(&self, node: &Node, first: Val, args: Vec<Val>) -> Result<Val, ExprError> {
        let at = node.at;
        match &node.kind {
            NodeKind::Member { prop, optional, .. } => member_value(first, prop, *optional, at),
            NodeKind::Unary { op, .. } => unary_value(op, first, at),
            NodeKind::Binary { op, right, .. } => self.run_binary(op, first, right, at),
            NodeKind::Cond { yes, no, .. } => match first {
                Val::Bool(true) => self.run(yes),
                Val::Bool(false) => self.run(no),
                v => Err(not_bool(&v, at)),
            },
            NodeKind::Call { .. } => self.call_value(first, args, at),
            // A leaf has no first child, so it is never a parent.
            _ => Ok(first),
        }
    }

    /// `l op right`; `&&`, `||` and `??` run `right` only when they need it.
    fn run_binary(&self, op: &str, l: Val, right: &Node, at: usize) -> Result<Val, ExprError> {
        match op {
            "&&" | "and" | "||" | "or" => {
                let Val::Bool(b) = l else {
                    return Err(not_bool(&l, at));
                };
                let short = if matches!(op, "&&" | "and") { !b } else { b };
                if short {
                    Ok(Val::Bool(b))
                } else {
                    self.run(right)
                }
            }
            "??" => match l {
                Val::Nil => self.run(right),
                v => Ok(v),
            },
            _ => {
                let r = tri!(self.run(right));
                binary_op(op, &l, &r).map_err(|m| ExprError::at(at, m))
            }
        }
    }

    /// Calls `callee` with `args`.
    fn call_value(&self, callee: Val, args: Vec<Val>, at: usize) -> Result<Val, ExprError> {
        match callee {
            Val::Func(b) => self.call(b, args).map_err(|m| ExprError::at(at, m)),
            Val::Nil => Err(ExprError::at(at, "invalid operation: cannot call nil")),
            v => Err(ExprError::at(
                at,
                format!(
                    "invalid operation: cannot call non-function of type {}",
                    v.type_name()
                ),
            )),
        }
    }

    fn call(&self, b: Builtin, args: Vec<Val>) -> Result<Val, String> {
        let sig = b.sig();
        // Arguments typed `string` take the zero value for nil and refuse
        // anything else, as reflection does in Go.
        let mut args = args;
        for (i, a) in args.iter_mut().enumerate() {
            if sig.params[i.min(sig.params.len() - 1)] == Param::Str {
                match a {
                    Val::Str(_) => {}
                    Val::Nil => *a = Val::Str(String::new()),
                    other => {
                        return Err(format!(
                            "reflect: Call using {} as type string",
                            other.type_name()
                        ));
                    }
                }
            }
        }
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Val::Nil);
        let text = |i: usize| match args.get(i) {
            Some(Val::Str(s)) => s.clone(),
            _ => String::new(),
        };
        Ok(match b {
            Builtin::Lower => match arg(0) {
                Val::Nil => Val::Str(String::new()),
                v => Val::Str(v.format().chars().flat_map(char::to_lowercase).collect()),
            },
            Builtin::Length => Val::Int(match arg(0) {
                Val::List(items) => items.len() as i64,
                Val::Map(map) => map.len() as i64,
                Val::Str(s) => s.len() as i64,
                _ => 0,
            }),
            Builtin::Isset => Val::Bool(match arg(0) {
                Val::Map(map) => map.contains_key(&text(1)),
                _ => false,
            }),
            Builtin::Changed => Val::Bool(match (arg(0), arg(2)) {
                (Val::Map(record), Val::Map(body)) => match body.get(&text(1)) {
                    Some(new) => !equal_values(new, record.get(&text(1)).unwrap_or(&Val::Nil)),
                    None => false,
                },
                _ => false,
            }),
            Builtin::GeoDistance => {
                let nums: Option<Vec<f64>> = (0..4).map(|i| arg(i).to_f64()).collect();
                Val::Float(nums.map_or(0.0, |n| geo_distance(n[0], n[1], n[2], n[3])))
            }
            Builtin::Strftime => Val::Str(self.strftime(&args).unwrap_or_default()),
            Builtin::Like => Val::Bool(like(&arg(0), &arg(1))),
            Builtin::Any(cmp) => Val::Bool(any_all(cmp, false, &arg(0), &arg(1))),
            Builtin::All(cmp) => Val::Bool(any_all(cmp, true, &arg(0), &arg(1))),
            Builtin::Exists => {
                Val::Bool(self.exists(&text(0), &text(1), &args[2.min(args.len())..]))
            }
            Builtin::Get => self.get_record(&text(0), &arg(1)),
        })
    }

    fn strftime(&self, args: &[Val]) -> Option<String> {
        let db = self.ctx.db?;
        let Val::Str(_) = args.first()? else {
            return None;
        };
        let params: Vec<Value> = args.iter().map(Val::to_param).collect::<Option<_>>()?;
        let marks: Vec<String> = (1..=params.len()).map(|i| format!("?{i}")).collect();
        let row = db
            .first_row(
                &format!("SELECT strftime({}) AS v", marks.join(", ")),
                &params,
            )
            .ok()??;
        match row.get("v")? {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        }
    }

    fn exists(&self, table: &str, filter: &str, args: &[Val]) -> bool {
        let Some(db) = self.ctx.db else { return false };
        if go::validate_table_name(table).is_err() {
            return false;
        }
        // Every `?` becomes a numbered placeholder; missing arguments are null.
        let mut sql = String::new();
        let mut params = Vec::new();
        for c in filter.chars() {
            if c == '?' {
                let Some(p) = args
                    .get(params.len())
                    .map_or(Some(Value::Null), Val::to_param)
                else {
                    return false;
                };
                params.push(p);
                sql.push_str(&format!("?{}", params.len()));
            } else {
                sql.push(c);
            }
        }
        let sql = format!(
            "SELECT 1 FROM {} WHERE {sql} LIMIT 1",
            moul_store::schema::quote(table)
        );
        matches!(db.first_row(&sql, &params), Ok(Some(_)))
    }

    /// `get(table, id)`: the record as the Go engine reads it back, or nil.
    fn get_record(&self, table: &str, id: &Val) -> Val {
        let Some(db) = self.ctx.db else {
            return Val::Nil;
        };
        let Val::Str(id) = id else { return Val::Nil };
        if id.is_empty() || go::validate_table_name(table).is_err() {
            return Val::Nil;
        }
        let Some(collection) = db.collection(table) else {
            return Val::Nil;
        };
        let sql = format!(
            "SELECT * FROM {} WHERE id = ?1 LIMIT 1",
            moul_store::schema::quote(table)
        );
        match db.first_row(&sql, &[Value::String(id.clone())]) {
            Ok(Some(row)) => normalize_record(&collection, &row),
            _ => Val::Nil,
        }
    }
}

fn not_bool(v: &Val, at: usize) -> ExprError {
    ExprError::at(
        at,
        format!(
            "interface conversion: interface {{}} is {}, not bool",
            v.assert_name()
        ),
    )
}

/// The type of a member of a value of the type `base`.
fn check_member(base: Nature, prop: &str, method: bool, at: usize) -> Result<Nature, ExprError> {
    match base {
        Nature::Unknown | Nature::Map => Ok(Nature::Unknown),
        Nature::Nil => Err(ExprError::at(at, format!("type nil has no field {prop}"))),
        Nature::List => Err(ExprError::at(
            at,
            "array elements can only be selected using an integer (got string)",
        )),
        other => {
            let what = if method { "method" } else { "field" };
            Err(ExprError::at(
                at,
                format!("type {} has no {what} {prop}", other.name()),
            ))
        }
    }
}

/// The type of a unary operation on a value of the type `n`.
fn check_unary(op: &str, n: Nature, at: usize) -> Result<Nature, ExprError> {
    match op {
        "!" | "not" if n == Nature::Bool || n.unknown() => Ok(Nature::Bool),
        "-" | "+" if n.number() => Ok(n),
        "-" | "+" if n.unknown() => Ok(Nature::Unknown),
        _ => Err(ExprError::at(
            at,
            format!("invalid operation: {op} (mismatched type {})", n.name()),
        )),
    }
}

/// The error for calling `name`, of the type `n`, which is no function.
fn not_callable(name: &str, n: Nature, at: usize) -> ExprError {
    if n == Nature::Nil {
        return ExprError::at(at, format!("{name} is nil; cannot call nil as function"));
    }
    ExprError::at(at, format!("{} is not callable", n.name()))
}

/// Why `count` arguments are wrong for `sig`, if they are.
fn argument_count_error(name: &str, sig: &Sig, count: usize) -> Option<String> {
    let n = sig.params.len();
    if sig.variadic {
        (count < n - 1).then(|| format!("not enough arguments to call {name}"))
    } else if count > n {
        Some(format!("too many arguments to call {name}"))
    } else if count < n {
        Some(format!("not enough arguments to call {name}"))
    } else {
        None
    }
}

/// Checks that argument `i`, of the type `a`, fits its parameter.
fn check_argument(name: &str, sig: &Sig, i: usize, a: Nature, at: usize) -> Result<(), ExprError> {
    let param = sig.params[i.min(sig.params.len() - 1)];
    if param == Param::Any || a.unknown() || a == Nature::Str {
        return Ok(());
    }
    if a == Nature::Nil {
        return Err(ExprError::at(
            at,
            format!("cannot use nil as argument (type string) to call {name}"),
        ));
    }
    Err(ExprError::at(
        at,
        format!(
            "cannot use {} as argument (type string) to call {name} ",
            a.name()
        ),
    ))
}

/// The member `prop` of `base`; nil for an optional member of nil.
fn member_value(base: Val, prop: &str, optional: bool, at: usize) -> Result<Val, ExprError> {
    if optional && matches!(base, Val::Nil) {
        return Ok(Val::Nil);
    }
    fetch(&base, prop).map_err(|m| ExprError::at(at, m))
}

/// A unary operation on `v`.
fn unary_value(op: &str, v: Val, at: usize) -> Result<Val, ExprError> {
    Ok(match (op, v) {
        ("!" | "not", Val::Bool(b)) => Val::Bool(!b),
        ("!" | "not", v) => return Err(not_bool(&v, at)),
        ("-", Val::Int(i)) => Val::Int(i.wrapping_neg()),
        ("-", Val::Float(f)) => Val::Float(-f),
        ("+", v @ (Val::Int(_) | Val::Float(_))) => v,
        (op, v) => {
            return Err(ExprError::at(
                at,
                format!("invalid operation: {op} {}", v.type_name()),
            ));
        }
    })
}

/// expr-lang's `runtime.Fetch` for a property name.
fn fetch(base: &Val, prop: &str) -> Result<Val, String> {
    match base {
        Val::Map(map) => Ok(map.get(prop).cloned().unwrap_or(Val::Nil)),
        Val::List(_) | Val::Str(_) => Err("invalid operation: int(string)".into()),
        other => Err(format!("cannot fetch {prop} from {}", other.type_name())),
    }
}

/// The Go helper `resolveSingleField`.
fn field(v: &Val, name: &str) -> Val {
    match v {
        Val::Map(map) => map.get(name).cloned().unwrap_or(Val::Nil),
        _ => Val::Nil,
    }
}

fn binary_op(op: &str, l: &Val, r: &Val) -> Result<Val, String> {
    let invalid = || {
        format!(
            "invalid operation: {} {op} {}",
            l.type_name(),
            r.type_name()
        )
    };
    Ok(match op {
        "==" => Val::Bool(equal(l, r)),
        "!=" => Val::Bool(!equal(l, r)),
        "<" | ">" | "<=" | ">=" => {
            let ord = match (l, r) {
                (Val::Int(a), Val::Int(b)) => a.partial_cmp(b),
                (Val::Str(a), Val::Str(b)) => a.partial_cmp(b),
                _ => match (l, r) {
                    (Val::Int(_) | Val::Float(_), Val::Int(_) | Val::Float(_)) => l
                        .to_f64()
                        .unwrap_or(0.0)
                        .partial_cmp(&r.to_f64().unwrap_or(0.0)),
                    _ => return Err(invalid()),
                },
            };
            use std::cmp::Ordering::{Equal, Greater, Less};
            Val::Bool(match op {
                "<" => ord == Some(Less),
                ">" => ord == Some(Greater),
                "<=" => matches!(ord, Some(Less | Equal)),
                _ => matches!(ord, Some(Greater | Equal)),
            })
        }
        "+" | "-" => match (l, r) {
            (Val::Int(a), Val::Int(b)) => Val::Int(if op == "+" {
                a.wrapping_add(*b)
            } else {
                a.wrapping_sub(*b)
            }),
            (Val::Int(_) | Val::Float(_), Val::Int(_) | Val::Float(_)) => {
                let (a, b) = (l.to_f64().unwrap_or(0.0), r.to_f64().unwrap_or(0.0));
                Val::Float(if op == "+" { a + b } else { a - b })
            }
            (Val::Str(a), Val::Str(b)) if op == "+" => Val::Str(format!("{a}{b}")),
            _ => return Err(invalid()),
        },
        "in" => Val::Bool(match r {
            Val::Nil => false,
            Val::List(items) => items.iter().any(|i| equal(i, l)),
            Val::Map(map) => match l {
                Val::Str(k) => map.contains_key(k),
                Val::Nil => map.contains_key(""),
                other => {
                    return Err(format!(
                        "reflect.Value.MapIndex: value of type {} is not assignable to type string",
                        other.type_name()
                    ));
                }
            },
            other => {
                return Err(format!(
                    "operator \"in\" not defined on {}",
                    other.type_name()
                ));
            }
        }),
        "contains" | "startsWith" | "endsWith" => match (l, r) {
            (Val::Nil, _) | (_, Val::Nil) => Val::Bool(false),
            (Val::Str(a), Val::Str(b)) => Val::Bool(match op {
                "contains" => a.contains(b.as_str()),
                "startsWith" => a.starts_with(b.as_str()),
                _ => a.ends_with(b.as_str()),
            }),
            (Val::Str(_), other) | (other, _) => {
                return Err(format!(
                    "interface conversion: interface {{}} is {}, not string",
                    other.type_name()
                ));
            }
        },
        _ => return Err(format!("operator {op} is not supported in rules")),
    })
}

/// The `any_*` and `all_*` helpers. An `all_*` over nothing is false.
fn any_all(cmp: Cmp, all: bool, a: &Val, b: &Val) -> bool {
    let items = a.to_slice();
    if all && items.is_empty() {
        return false;
    }
    let numeric = |f: fn(f64, f64) -> bool| -> bool {
        let Some(y) = b.to_f64() else { return false };
        if all {
            items.iter().all(|i| i.to_f64().is_some_and(|x| f(x, y)))
        } else {
            items.iter().any(|i| i.to_f64().is_some_and(|x| f(x, y)))
        }
    };
    let each = |f: &dyn Fn(&Val) -> bool| {
        if all {
            items.iter().all(f)
        } else {
            items.iter().any(f)
        }
    };
    match cmp {
        Cmp::Eq => each(&|i| equal_values(i, b)),
        Cmp::Neq => each(&|i| !equal_values(i, b)),
        Cmp::Like => each(&|i| like(i, b)),
        Cmp::NotLike => each(&|i| !like(i, b)),
        Cmp::Gt => numeric(|x, y| x > y),
        Cmp::Gte => numeric(|x, y| x >= y),
        Cmp::Lt => numeric(|x, y| x < y),
        Cmp::Lte => numeric(|x, y| x <= y),
    }
}

/// Great-circle distance in kilometres between two longitude/latitude
/// points.
fn geo_distance(lon_a: f64, lat_a: f64, lon_b: f64, lat_b: f64) -> f64 {
    let rad = std::f64::consts::PI / 180.0;
    let (la1, lo1, la2, lo2) = (lat_a * rad, lon_a * rad, lat_b * rad, lon_b * rad);
    let (dlat, dlon) = (la2 - la1, lo2 - lo1);
    let a = (dlat / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlon / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

/// A row as the Go engine's `get(` returns it: every column read as text,
/// then numbers, booleans, JSON and many-relations decoded by field type;
/// credential columns removed.
fn normalize_record(collection: &Collection, row: &Map) -> Val {
    let mut record: BTreeMap<String, Val> = row
        .iter()
        .map(|(k, v)| {
            let text = match v {
                Value::Null => Val::Nil,
                Value::String(s) => Val::Str(s.clone()),
                Value::Number(n) => Val::Str(match (n.as_i64(), n.as_u64()) {
                    (Some(i), _) => i.to_string(),
                    (_, Some(u)) => u.to_string(),
                    _ => go::format_float(n.as_f64().unwrap_or(0.0)),
                }),
                Value::Bool(b) => Val::Str(b.to_string()),
                other => Val::Str(other.to_string()),
            };
            (k.clone(), text)
        })
        .collect();
    for key in ["passwordHash", "otpCode", "otpExpiresAt", "passkeys"] {
        record.remove(key);
    }
    for f in &collection.fields {
        let many = f.kind == FieldType::Relation
            && f.relation.as_ref().is_some_and(|r| r.cardinality == "M:N");
        let Some(Val::Str(s)) = record.get(&f.name) else {
            if many && matches!(record.get(&f.name), None | Some(Val::Nil)) {
                record.insert(f.name.clone(), Val::List(Rc::default()));
            }
            continue;
        };
        let decoded = match f.kind {
            FieldType::Number => go::parse_float(s).map(Val::Float),
            FieldType::Bool => Some(Val::Bool(s == "1" || s == "true")),
            FieldType::Json | FieldType::File if !s.is_empty() => serde_json::from_str::<Value>(s)
                .ok()
                .map(|v| Val::from_json(&v)),
            FieldType::Relation if many => {
                let ids = match serde_json::from_str::<Option<Vec<String>>>(s) {
                    Ok(Some(ids)) if s != "null" => ids,
                    _ => Vec::new(),
                };
                Some(Val::List(Rc::new(ids.into_iter().map(Val::Str).collect())))
            }
            _ => None,
        };
        if let Some(v) = decoded {
            record.insert(f.name.clone(), v);
        }
    }
    Val::Map(Rc::new(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_follows_go() {
        let s = |x: &str| Val::Str(x.into());
        assert!(like(&s("Lorem Ipsum"), &s("Lorem%")));
        assert!(like(&s("Lorem Ipsum dolor"), &s("ipsum")));
        // Without `%` the pattern may match anywhere.
        assert!(like(&s("Lorem"), &s("o_e")));
        assert!(!like(&s("Lorem"), &s("o_e%")));
        assert!(like(&s("Lorem"), &s("L_r%")));
        assert!(!like(&s("a\nb"), &s("a%b")));
        assert!(like(&s("a.b"), &s("a.b")));
        assert!(!like(&s("axb"), &s("%a.b%")));
        assert!(!like(&Val::Float(1.0), &s("1")));
    }

    #[test]
    fn equal_values_follows_go() {
        assert!(equal_values(&Val::Str("5".into()), &Val::Int(5)));
        assert!(equal_values(&Val::Bool(true), &Val::Str("true".into())));
        assert!(!equal_values(&Val::Nil, &Val::Str(String::new())));
        assert!(equal(&Val::Float(2.0), &Val::Int(2)));
        assert!(!equal(&Val::Str("2".into()), &Val::Int(2)));
    }
}
