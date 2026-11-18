//! Go's `BuildFilterSQLWithOptions`, `BuildSortSQLWithOptions` and the
//! field helpers: a filter or rule compiled to a SQL `WHERE`, and a sort
//! string to `ORDER BY` terms.

use std::cell::RefCell;

use moul_store::schema::{Collection, CollectionKind, Field, FieldType, quote};
use serde_json::Value;

use crate::tokenize::{Kind, Token, tokenize};
use crate::translate::is_date_macro;
use crate::{Error, MAX_DEPTH, Map, go};

/// Columns every kind of collection may have, valid in any filter or sort.
const STANDARD_FIELDS: &[&str] = &[
    "id",
    "created",
    "updated",
    "createdAt",
    "updatedAt",
    "created_at",
    "updated_at",
    "inserted_at",
    "scheduled_at",
    "state",
    "worker",
    "queue",
    "attempt",
    "errors",
    "max_attempts",
    "priority",
    "args",
    "meta",
    "tags",
    "username",
    "email",
    "emailVisibility",
    "verified",
    "tokenKey",
    "passwordHash",
    "lastResetSentAt",
    "lastVerificationSentAt",
    "avatar",
    "name",
    "properties",
    "visit_token",
    "visitor_token",
    "user_id",
    "ip",
    "user_agent",
    "referrer",
    "landing_page",
];

/// Columns no client-supplied filter or sort may reference, lowercase:
/// credentials, one-time codes, reset tokens and linked OAuth identities.
const SECRET_FIELDS: &[&str] = &[
    "passwordhash",
    "tokenkey",
    "otpcode",
    "otpexpiresat",
    "passkeys",
    "resettoken",
    "resettokenexpiresat",
    "oauthproviders",
];

/// The alias of a top-level relation or `@collection` subquery. Deeper
/// subqueries add their depth: `_sub1`, `_sub2`, ...
const SUBQUERY_ALIAS: &str = "_sub";

/// Whether `name` may be filtered or sorted on in `c`: a standard column,
/// one of its fields, or `relation.column` through one of its relation
/// fields. Names compare with case.
pub fn is_valid_field(name: &str, c: &Collection) -> bool {
    if STANDARD_FIELDS.contains(&name) {
        return true;
    }
    if let Some((first, _)) = name.split_once('.')
        && c.fields
            .iter()
            .any(|f| f.name == first && f.kind == FieldType::Relation)
    {
        return true;
    }
    c.fields.iter().any(|f| f.name == name)
}

/// Whether a caller who is not an admin may never probe the column `name`
/// of `c`: credentials and tokens everywhere, and `email` on an auth
/// collection.
pub fn is_secret_field(name: &str, c: &Collection) -> bool {
    let lower = name.to_lowercase();
    SECRET_FIELDS.contains(&lower.as_str()) || (lower == "email" && c.kind == CollectionKind::Auth)
}

/// The columns a rule pins to the caller: every direct comparison
/// `<column> = @request.auth.id` (or `auth.id`, `@auth.id`), in either
/// order. A write by someone who is not an admin may only set such a column
/// to the caller's own id.
pub fn owner_fields(rule: &str) -> Vec<String> {
    let Ok(tokens) = tokenize(rule) else {
        return Vec::new();
    };
    let is_auth_id = |t: &Token| {
        t.is(Kind::Identifier)
            && matches!(
                t.value.as_str(),
                "@request.auth.id" | "auth.id" | "@auth.id"
            )
    };
    let is_column = |t: &Token| {
        t.is(Kind::Identifier)
            && go::is_ident(&t.value)
            && !["id", "true", "false", "null"]
                .iter()
                .any(|w| t.value.eq_ignore_ascii_case(w))
    };
    let mut fields: Vec<String> = Vec::new();
    for w in tokens.windows(3) {
        let (left, op, right) = (&w[0], &w[1], &w[2]);
        if !op.is(Kind::Operator) || (op.value != "=" && op.value != "==") {
            continue;
        }
        let col = if is_column(left) && is_auth_id(right) {
            &left.value
        } else if is_auth_id(left) && is_column(right) {
            &right.value
        } else {
            continue;
        };
        if !fields.contains(col) {
            fields.push(col.clone());
        }
    }
    fields
}

/// Finds a collection by name: how [`filter_sql`] and [`sort_sql`] reach the
/// target of a relation.
pub type LoadCollection<'a> = &'a dyn Fn(&str) -> Option<Collection>;

// ---------------------------------------------------------------- sort

/// Options for [`sort_sql`].
#[derive(Clone, Copy, Default)]
pub struct SortOptions<'a> {
    /// A client-supplied sort (`?sort=`): secret columns are refused, and so
    /// is a relation path into a collection whose rows are not public.
    pub untrusted: bool,
    /// Finds a relation's target collection, so its columns can be checked.
    /// Without it relation paths are refused.
    pub load: Option<LoadCollection<'a>>,
}

/// Go's `BuildSortSQLWithOptions`: a comma-separated sort such as
/// `-created,title`, `@random` or `-author.name`, as `ORDER BY` terms.
/// Every column is checked against its collection before it is written.
pub fn sort_sql(
    sort: &str,
    collection: &Collection,
    opts: &SortOptions<'_>,
) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    if sort.trim().is_empty() {
        return Ok(out);
    }
    for part in sort.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (direction, name) = match part.strip_prefix('-') {
            Some(rest) => ("DESC", rest),
            None => ("ASC", part.strip_prefix('+').unwrap_or(part)),
        };
        if name == "@random" {
            out.push("RANDOM()".to_owned());
            continue;
        }
        if name.contains('.') {
            out.push(relation_sort(name, direction, collection, opts)?);
            continue;
        }
        if !go::is_ident(name) || !is_valid_field(name, collection) {
            return Err(Error::new(format!(
                "invalid sort field {}",
                go::quote(name)
            )));
        }
        if opts.untrusted && is_secret_field(name, collection) {
            return Err(Error::new(format!(
                "sorting by {} is not allowed",
                go::quote(name)
            )));
        }
        out.push(format!("{name} {direction}"));
    }
    Ok(out)
}

fn relation_sort(
    path: &str,
    direction: &str,
    c: &Collection,
    opts: &SortOptions<'_>,
) -> Result<String, Error> {
    let invalid = || Error::new(format!("invalid sort field {}", go::quote(path)));
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() != 2 || !go::is_ident(parts[0]) || !go::is_ident(parts[1]) {
        return Err(invalid());
    }
    let (rel_field, relation) = relation_field(c, parts[0]).ok_or_else(invalid)?;
    let target = load_target(opts.load, &relation.target_moul)
        .map_err(|e| Error::new(format!("invalid sort field {}: {e}", go::quote(path))))?;
    let column = parts[1];
    if !is_valid_field(column, &target) {
        return Err(invalid());
    }
    if opts.untrusted {
        if is_secret_field(column, &target) {
            return Err(Error::new(format!(
                "sorting by {} is not allowed",
                go::quote(path)
            )));
        }
        if !target.rules.list_rule.trim().is_empty() {
            return Err(Error::new(format!(
                "sorting by {} is not allowed: {} records are not public",
                go::quote(path),
                target.name
            )));
        }
    }
    let table = quote(&target.name);
    let outer = format!("{}.{}", quote(&c.name), quote(&rel_field.name));
    let col = format!("{SUBQUERY_ALIAS}.{}", quote(column));
    match relation.cardinality.as_str() {
        "1:1" | "1:N" => Ok(format!(
            "(SELECT {col} FROM {table} AS {SUBQUERY_ALIAS} WHERE {SUBQUERY_ALIAS}.id = {outer}) {direction}"
        )),
        "M:N" => Ok(format!(
            "(SELECT {col} FROM {table} AS {SUBQUERY_ALIAS} JOIN json_each({outer}) WHERE {SUBQUERY_ALIAS}.id = json_each.value LIMIT 1) {direction}"
        )),
        other => Err(Error::new(format!(
            "unsupported relation cardinality {}",
            go::quote(other)
        ))),
    }
}

/// The relation field named `name` of `c` that has a target, with it.
fn relation_field<'c>(
    c: &'c Collection,
    name: &str,
) -> Option<(&'c Field, &'c moul_store::schema::Relation)> {
    c.fields
        .iter()
        .find(|f| f.name == name && f.kind == FieldType::Relation && f.relation.is_some())
        .and_then(|f| Some((f, f.relation.as_ref()?)))
}

fn load_target(load: Option<LoadCollection<'_>>, name: &str) -> Result<Collection, String> {
    let Some(load) = load else {
        return Err("relation paths are not supported here".into());
    };
    go::validate_table_name(name)?;
    load(name).ok_or_else(|| format!("unknown relation target {}: not found", go::quote(name)))
}

// ---------------------------------------------------------------- filter

/// Options for [`filter_sql`].
#[derive(Clone, Copy, Default)]
pub struct FilterOptions<'a> {
    /// Prefix of placeholder names (`<prefix>p1`, ...), so several compiled
    /// filters can be bound in one query.
    pub param_prefix: &'a str,
    /// A client-supplied filter (`?filter=`): it may not use `@collection`
    /// or secret columns, and a relation path only matches target rows the
    /// caller may list (the target's list rule joins the subquery).
    pub untrusted: bool,
    /// Finds a relation's target collection, so its columns can be checked.
    /// Without it relation paths are refused.
    pub load: Option<LoadCollection<'a>>,
    /// The current time in milliseconds since the Unix epoch, for `@now`
    /// and the other date macros.
    pub now_ms: i64,
    /// The blind index of a value, for equality on a searchable `cloak`
    /// field (the Go engine's HMAC-SHA256 of the text). Without it the
    /// index is empty, as in a Go engine without an encryption key.
    pub cloak_hash: Option<&'a dyn Fn(&str) -> String>,
}

/// A compiled filter: SQL for a `WHERE` and the values of its placeholders.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlFilter {
    /// The condition. Placeholders are named, `:p1` or `:<prefix>p1`, as
    /// rusqlite binds them.
    pub sql: String,
    /// Placeholder values in the order they were made, keyed without the `:`.
    pub params: Vec<(String, Value)>,
}

/// Go's `BuildFilterSQLWithOptions`: a filter (or a list rule) as SQL. A
/// blank filter, or one of comments only, is `Ok(None)`.
///
/// `auth` is the caller (`@request.auth.*`; a missing value is `''`) and
/// `request` the request, of which only `query` and `headers` are read.
///
/// Columns are written bare, so the SQL runs against the collection's own
/// table; relation paths and `@collection` become `EXISTS` subqueries.
pub fn filter_sql(
    filter: &str,
    collection: &Collection,
    auth: Option<&Map>,
    request: Option<&Map>,
    opts: &FilterOptions<'_>,
) -> Result<Option<SqlFilter>, Error> {
    if filter.trim().is_empty() {
        return Ok(None);
    }
    let tokens =
        tokenize(filter).map_err(|e| Error::new(format!("failed to tokenize filter: {e}")))?;
    if tokens.is_empty() {
        return Ok(None);
    }
    let state = RefCell::new(Params {
        prefix: opts.param_prefix.to_owned(),
        counter: 0,
        values: Vec::new(),
    });
    let mut b = Builder {
        tokens: &tokens,
        pos: 0,
        collection,
        auth,
        request,
        opts,
        untrusted: opts.untrusted,
        state: &state,
        depth: 0,
        nesting: 0,
        self_ref: String::new(),
    };
    let sql = b.or()?;
    if let Some(t) = tokens.get(b.pos) {
        return Err(Error::new(format!(
            "unexpected token {} at position {}",
            go::quote(&t.value),
            b.pos
        )));
    }
    Ok(Some(SqlFilter {
        sql,
        params: state.into_inner().values,
    }))
}

/// Placeholders shared by a builder and the builders of nested rules.
struct Params {
    prefix: String,
    counter: usize,
    values: Vec<(String, Value)>,
}

/// A literal operand, typed as in Go: number tokens are `int64` or
/// `float64`, values read from the auth record or the request are JSON.
#[derive(Clone, Debug)]
enum Lit {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Json(Value),
}

impl Lit {
    fn to_json(&self) -> Value {
        match self {
            Lit::Int(i) => Value::from(*i),
            Lit::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Lit::Str(s) => Value::String(s.clone()),
            Lit::Bool(b) => Value::Bool(*b),
            Lit::Json(v) => v.clone(),
        }
    }

    /// Go's `%v`.
    fn format(&self) -> String {
        match self {
            Lit::Int(i) => i.to_string(),
            Lit::Float(f) => go::format_float(*f),
            Lit::Str(s) => s.clone(),
            Lit::Bool(b) => b.to_string(),
            Lit::Json(v) => go::format_json(v),
        }
    }
}

/// A resolved token: SQL text, a literal value, or a column.
#[derive(Clone, Debug, Default)]
struct Operand {
    expr: String,
    val: Option<Lit>,
    col: bool,
}

impl Operand {
    fn lit(val: Option<Lit>) -> Operand {
        Operand {
            expr: String::new(),
            val,
            col: false,
        }
    }

    /// No column, no value and no SQL: the `null` literal.
    fn is_null(&self) -> bool {
        self.val.is_none() && !self.col && self.expr.is_empty()
    }
}

struct Builder<'b> {
    tokens: &'b [Token],
    pos: usize,
    collection: &'b Collection,
    auth: Option<&'b Map>,
    request: Option<&'b Map>,
    opts: &'b FilterOptions<'b>,
    untrusted: bool,
    state: &'b RefCell<Params>,
    /// 0 at the top level, one more for each nested target rule.
    depth: usize,
    /// How many parentheses and `!` are open, counting those around a
    /// nested target rule. Past `MAX_DEPTH` the filter is refused.
    nesting: usize,
    /// How this builder's own table is referenced: empty at the top level
    /// (by name), or the alias of the subquery a nested rule sits in.
    self_ref: String,
}

type Sql = Result<String, Error>;

fn err(message: impl Into<String>) -> Error {
    Error::new(message)
}

impl Builder<'_> {
    fn sub_alias(&self) -> String {
        if self.depth == 0 {
            SUBQUERY_ALIAS.to_owned()
        } else {
            format!("{SUBQUERY_ALIAS}{}", self.depth)
        }
    }

    fn param(&self, value: Value) -> String {
        let mut s = self.state.borrow_mut();
        s.counter += 1;
        let name = format!("{}p{}", s.prefix, s.counter);
        s.values.push((name.clone(), value));
        format!(":{name}")
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn or(&mut self) -> Sql {
        let mut left = self.and()?;
        while let Some(t) = self.peek() {
            if t.is(Kind::Logical) && (t.value == "||" || t.value.eq_ignore_ascii_case("or")) {
                self.pos += 1;
                let right = self.and()?;
                left = format!("({left} OR {right})");
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn and(&mut self) -> Sql {
        let mut left = self.primary()?;
        while let Some(t) = self.peek() {
            if t.is(Kind::Logical) && (t.value == "&&" || t.value.eq_ignore_ascii_case("and")) {
                self.pos += 1;
                let right = self.primary()?;
                left = format!("({left} AND {right})");
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// Runs `f` one level deeper, or refuses the filter past `MAX_DEPTH`
    /// levels, before deep input can exhaust the stack.
    fn nested(&mut self, f: impl FnOnce(&mut Self) -> Sql) -> Sql {
        if self.nesting >= MAX_DEPTH {
            return Err(err("filter is nested too deeply"));
        }
        self.nesting += 1;
        let sql = f(self);
        self.nesting -= 1;
        sql
    }

    fn primary(&mut self) -> Sql {
        let Some(t) = self.peek() else {
            return Err(err("unexpected end of filter expression"));
        };
        if t.is(Kind::Operator) && (t.value == "!" || t.value.eq_ignore_ascii_case("not")) {
            self.pos += 1;
            let sub = self.nested(Self::primary)?;
            return Ok(format!("(NOT {sub})"));
        }
        if t.is(Kind::Paren) && t.value == "(" {
            self.pos += 1;
            let inner = self.nested(Self::or)?;
            match self.peek() {
                Some(t) if t.is(Kind::Paren) && t.value == ")" => self.pos += 1,
                _ => return Err(err("missing closing parenthesis")),
            }
            return Ok(format!("({inner})"));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Sql {
        let Some(left_tok) = self.peek().cloned() else {
            return Err(err("unexpected end of filter expression"));
        };
        if left_tok.is(Kind::Identifier) && self.is_relation_path(&left_tok.value) {
            return self.relation_comparison();
        }
        if left_tok.is(Kind::Identifier) && left_tok.value.starts_with("@collection.") {
            return self.collection_subquery();
        }
        let left = self.resolve(&left_tok)?;
        self.pos += 1;
        let op = match self.peek() {
            Some(t) if t.is(Kind::Operator) => t.value.clone(),
            _ => {
                if left.col {
                    let e = &left.expr;
                    return Ok(format!("{e} IS NOT NULL AND {e} != 0 AND {e} != ''"));
                }
                if let Some(v) = &left.val {
                    return Ok(self.param(v.to_json()));
                }
                return Ok(left.expr);
            }
        };
        self.pos += 1;
        let Some(right_tok) = self.peek().cloned() else {
            return Err(err(format!("missing right operand for operator {op}")));
        };
        self.pos += 1;
        let right = self.resolve(&right_tok)?;
        self.format_comparison(left, &op, right)
    }

    fn collection_subquery(&mut self) -> Sql {
        let left = self.tokens[self.pos].value.clone();
        if self.untrusted {
            return Err(err("@collection references are not allowed in filters"));
        }
        self.pos += 1;
        let op = match self.peek() {
            Some(t) if t.is(Kind::Operator) => t.value.clone(),
            _ => return Err(err(format!("expected operator after {left}"))),
        };
        self.pos += 1;
        let Some(right_tok) = self.peek().cloned() else {
            return Err(err("missing right operand for @collection condition"));
        };
        self.pos += 1;

        let invalid = || {
            err(format!(
                "invalid @collection identifier {}",
                go::quote(&left)
            ))
        };
        let rest = &left["@collection.".len()..];
        let (table, column) = rest.split_once('.').ok_or_else(invalid)?;
        go::validate_table_name(table).map_err(|e| {
            err(format!(
                "invalid @collection identifier {}: {e}",
                go::quote(&left)
            ))
        })?;
        if !go::is_ident(column) {
            return Err(invalid());
        }

        let right = self.resolve(&right_tok)?;
        let mut cond = if right.col {
            self.qualified_outer(&right.expr)
        } else if let Some(v) = &right.val {
            self.param(v.to_json())
        } else {
            right.expr.clone()
        };
        let op = map_operator(&op);
        if (op == "LIKE" || op == "NOT LIKE")
            && !right.col
            && let Some(v) = &right.val
        {
            // Go binds the plain value too and leaves it unused.
            cond = self.param(Value::String(format!("%{}%", v.format())));
        }
        let alias = self.sub_alias();
        Ok(format!(
            "EXISTS (SELECT 1 FROM {} AS {alias} WHERE {alias}.{} {op} {cond})",
            quote(table),
            quote(column)
        ))
    }

    /// A bare column of this builder's table, qualified so it keeps meaning
    /// this row inside a deeper subquery.
    fn qualified_outer(&self, col: &str) -> String {
        if !go::is_ident(col) {
            return col.to_owned();
        }
        if !self.self_ref.is_empty() {
            return format!("{}.{}", self.self_ref, quote(col));
        }
        format!("{}.{}", quote(&self.collection.name), quote(col))
    }

    fn is_relation_path(&self, name: &str) -> bool {
        match name.split_once('.') {
            Some((first, _)) => relation_field(self.collection, first).is_some(),
            None => false,
        }
    }

    fn relation_comparison(&mut self) -> Sql {
        let left = self.tokens[self.pos].value.clone();
        self.pos += 1;
        let op = match self.peek() {
            Some(t) if t.is(Kind::Operator) => t.value.clone(),
            _ => {
                return Err(err(format!(
                    "expected operator after relational path {left}"
                )));
            }
        };
        self.pos += 1;
        let Some(right_tok) = self.peek().cloned() else {
            return Err(err(format!(
                "missing right operand for relational path {left}"
            )));
        };
        self.pos += 1;
        let right = self.resolve(&right_tok)?;

        let parts: Vec<&str> = left.split('.').collect();
        if parts.len() != 2 || !go::is_ident(parts[1]) {
            return Err(err(format!("invalid relational path {}", go::quote(&left))));
        }
        let (rel_name, column) = (parts[0], parts[1]);
        let Some((_, relation)) = relation_field(self.collection, rel_name) else {
            return Err(err(format!(
                "invalid relation field {} in path {}",
                go::quote(rel_name),
                go::quote(&left)
            )));
        };
        let relation = relation.clone();
        let target = load_target(self.opts.load, &relation.target_moul)
            .map_err(|e| err(format!("invalid relational path {}: {e}", go::quote(&left))))?;
        if !is_valid_field(column, &target) {
            return Err(err(format!(
                "invalid field {} in path {}",
                go::quote(column),
                go::quote(&left)
            )));
        }
        if self.untrusted && is_secret_field(column, &target) {
            return Err(err(format!(
                "filtering by {} is not allowed",
                go::quote(&left)
            )));
        }

        // A client filter only matches target rows the caller may list.
        let mut visibility = String::new();
        if self.untrusted {
            let rule = target.rules.list_rule.trim();
            if !rule.is_empty() {
                visibility = format!(" AND ({})", self.nested_rule(rule, &target)?);
            }
        }

        let alias = self.sub_alias();
        let table = format!("{} AS {alias}", quote(&target.name));
        let target_col = format!("{alias}.{}", quote(column));
        let outer_col = self.qualified_outer(rel_name);
        let op = map_operator(&op);

        let cond = if right.col {
            self.qualified_outer(&right.expr)
        } else if let Some(v) = &right.val {
            if op == "LIKE" || op == "NOT LIKE" {
                self.param(Value::String(format!("%{}%", v.format())))
            } else {
                self.param(v.to_json())
            }
        } else if !right.expr.is_empty() {
            right.expr.clone()
        } else {
            let check = if op == "=" { "IS NULL" } else { "IS NOT NULL" };
            return Ok(format!(
                "EXISTS (SELECT 1 FROM {table} WHERE {alias}.id = {outer_col} AND {target_col} {check}{visibility})"
            ));
        };
        match relation.cardinality.as_str() {
            "1:1" | "1:N" => Ok(format!(
                "EXISTS (SELECT 1 FROM {table} WHERE {alias}.id = {outer_col} AND {target_col} {op} {cond}{visibility})"
            )),
            "M:N" => Ok(format!(
                "EXISTS (SELECT 1 FROM {table} JOIN json_each({outer_col}) WHERE {alias}.id = json_each.value AND {target_col} {op} {cond}{visibility})"
            )),
            other => Err(err(format!(
                "unsupported relation cardinality {}",
                go::quote(other)
            ))),
        }
    }

    /// A target collection's (trusted) list rule, compiled in this
    /// builder's placeholder namespace for use inside its subquery.
    fn nested_rule(&self, rule: &str, target: &Collection) -> Sql {
        let tokens = tokenize(rule)
            .map_err(|e| err(format!("failed to tokenize {} list rule: {e}", target.name)))?;
        let mut nested = Builder {
            tokens: &tokens,
            pos: 0,
            collection: target,
            auth: self.auth,
            request: self.request,
            opts: self.opts,
            untrusted: false,
            state: self.state,
            depth: self.depth + 1,
            nesting: self.nesting,
            self_ref: self.sub_alias(),
        };
        let sql = nested.or().map_err(|e| {
            err(format!(
                "failed to translate {} list rule: {}",
                target.name,
                e.message()
            ))
        })?;
        if let Some(t) = tokens.get(nested.pos) {
            return Err(err(format!(
                "unexpected token {} in {} list rule",
                go::quote(&t.value),
                target.name
            )));
        }
        Ok(sql)
    }

    fn resolve(&self, tok: &Token) -> Result<Operand, Error> {
        match tok.kind {
            Kind::Number => {
                let v = &tok.value;
                Ok(Operand::lit(Some(if let Ok(i) = v.parse::<i64>() {
                    Lit::Int(i)
                } else if let Some(f) = v.parse::<f64>().ok().filter(|f| f.is_finite()) {
                    Lit::Float(f)
                } else {
                    Lit::Str(v.clone())
                })))
            }
            Kind::String => Ok(Operand::lit(Some(Lit::Str(tok.value.clone())))),
            Kind::Identifier => self.resolve_identifier(&tok.value),
            _ => Err(err(format!("unexpected token {}", go::quote(&tok.value)))),
        }
    }

    fn resolve_identifier(&self, value: &str) -> Result<Operand, Error> {
        if value.eq_ignore_ascii_case("true") {
            return Ok(Operand::lit(Some(Lit::Bool(true))));
        }
        if value.eq_ignore_ascii_case("false") {
            return Ok(Operand::lit(Some(Lit::Bool(false))));
        }
        if value.eq_ignore_ascii_case("null") {
            return Ok(Operand::lit(None));
        }
        // The caller: the last part of auth.x, @request.auth.x or @auth.x.
        if value.starts_with("auth.")
            || value.starts_with("@request.auth.")
            || value.starts_with("@auth.")
        {
            let key = value.rsplit('.').next().unwrap_or_default();
            let found = self.auth.and_then(|a| a.get(key)).filter(|v| !v.is_null());
            return Ok(Operand::lit(Some(
                found.map_or(Lit::Str(String::new()), |v| Lit::Json(v.clone())),
            )));
        }
        for (prefix, part) in [
            ("@request.query.", "query"),
            ("@request.headers.", "headers"),
        ] {
            if let Some(name) = value.strip_prefix(prefix) {
                let found = self
                    .request
                    .and_then(|r| r.get(part))
                    .and_then(Value::as_object)
                    .and_then(|m| m.get(name))
                    .filter(|v| !v.is_null());
                return Ok(Operand::lit(found.map(|v| Lit::Json(v.clone()))));
            }
        }
        if let Some(m) = value.strip_prefix('@')
            && is_date_macro(m)
        {
            return Ok(Operand::lit(
                go::date_macro(m, self.opts.now_ms).map(Lit::Str),
            ));
        }
        // Modifiers: col:lower, col:length, col:isset.
        if let Some((base, modifier)) = value.split_once(':') {
            if !go::is_ident(base) || !is_valid_field(base, self.collection) {
                return Err(err(format!("invalid field {}", go::quote(base))));
            }
            if self.untrusted && is_secret_field(base, self.collection) {
                return Err(err(format!(
                    "filtering by {} is not allowed",
                    go::quote(base)
                )));
            }
            match modifier {
                "lower" => {
                    return Ok(Operand {
                        expr: format!("LOWER({base})"),
                        val: None,
                        col: true,
                    });
                }
                "length" => {
                    return Ok(Operand {
                        expr: format!("LENGTH({base})"),
                        val: None,
                        col: true,
                    });
                }
                "isset" => {
                    return Ok(Operand {
                        expr: format!("({base} IS NOT NULL)"),
                        val: None,
                        col: false,
                    });
                }
                _ => {}
            }
        }
        if go::is_ident(value) && is_valid_field(value, self.collection) {
            if self.untrusted && is_secret_field(value, self.collection) {
                return Err(err(format!(
                    "filtering by {} is not allowed",
                    go::quote(value)
                )));
            }
            return Ok(Operand {
                expr: value.to_owned(),
                val: None,
                col: true,
            });
        }
        Err(err(format!("invalid field {}", go::quote(value))))
    }

    /// The `cloak` field a column refers to, compared without case.
    fn cloak_field(&self, col: &str) -> Option<&Field> {
        self.collection
            .fields
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(col) && f.kind == FieldType::Cloak)
    }

    fn format_comparison(&self, left: Operand, op: &str, right: Operand) -> Sql {
        let op = map_operator(op);

        let (cloak, target, cloak_left) = if left.col {
            (self.cloak_field(&left.expr), right.val.as_ref(), true)
        } else if right.col {
            (self.cloak_field(&right.expr), left.val.as_ref(), false)
        } else {
            (None, None, false)
        };
        if let Some(f) = cloak {
            if !f.searchable {
                return Err(err(format!(
                    "cloak field {} is not searchable",
                    go::quote(&f.name)
                )));
            }
            if op != "=" && op != "!=" {
                return Err(err(format!(
                    "cloak field {} only supports '=' and '!=' operators",
                    go::quote(&f.name)
                )));
            }
            let hash_col = quote(&format!("{}Hash", f.name));
            let Some(target) = target else {
                let check = if op == "=" { "IS NULL" } else { "IS NOT NULL" };
                return Ok(format!("{hash_col} {check}"));
            };
            let hash = self
                .opts
                .cloak_hash
                .map(|h| h(&target.format()))
                .unwrap_or_default();
            let p = self.param(Value::String(hash));
            return Ok(if cloak_left {
                format!("{hash_col} {op} {p}")
            } else {
                format!("{p} {op} {hash_col}")
            });
        }

        // Comparisons with null.
        for (null_side, other) in [(&right, &left), (&left, &right)] {
            if null_side.is_null() && (op == "=" || op == "!=") {
                let check = if op == "=" { "IS NULL" } else { "IS NOT NULL" };
                if other.col {
                    return Ok(format!("{} {check}", other.expr));
                }
                let p = self.param(other.val.as_ref().map_or(Value::Null, Lit::to_json));
                return Ok(format!("{p} {check}"));
            }
        }

        let left_sql = if left.col {
            left.expr
        } else if let Some(v) = &left.val {
            self.param(v.to_json())
        } else {
            left.expr
        };
        let right_sql = if right.col {
            right.expr
        } else if let Some(v) = &right.val {
            if op == "LIKE" || op == "NOT LIKE" {
                self.param(Value::String(format!("%{}%", v.format())))
            } else {
                self.param(v.to_json())
            }
        } else {
            right.expr
        };
        Ok(format!("{left_sql} {op} {right_sql}"))
    }
}

fn map_operator(op: &str) -> &str {
    match op {
        "=" | "==" | "?=" => "=",
        "!=" | "?!=" => "!=",
        "~" | "?~" => "LIKE",
        "!~" | "?!~" => "NOT LIKE",
        ">" | "?>" => ">",
        ">=" | "?>=" => ">=",
        "<" | "?<" => "<",
        "<=" | "?<=" => "<=",
        other => other,
    }
}
