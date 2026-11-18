//! Go's `rules.Translate`: a rule becomes an expression in the syntax of
//! expr-lang, which the Go engine compiled and ran. The evaluator parses
//! that expression with its own expr-lang subset (`crate::expr`), so a rule
//! means exactly what it meant to the Go engine, quirks included.

use crate::go;
use crate::tokenize::{Kind, Token, tokenize};

/// One `@collection.<table>.<field> <op> <value>` condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Condition {
    pub field: String,
    pub operator: String,
    pub value: String,
    /// The value names a variable (a path) rather than a literal.
    pub is_path: bool,
}

/// The `@collection` conditions on one table (or one `table:alias`), which
/// must all hold for the same row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Group {
    /// `table` or `table:alias`, as written.
    pub key: String,
    pub table: String,
    pub conditions: Vec<Condition>,
}

impl Group {
    /// The variable the expression reads the group's result from.
    pub fn placeholder(&self) -> String {
        format!("exists_group_{}", self.key.replace(':', "_"))
    }
}

/// Translates a rule. An empty rule translates to an empty expression.
pub(crate) fn translate(rule: &str) -> Result<(String, Vec<Group>), String> {
    if rule.is_empty() {
        return Ok((String::new(), Vec::new()));
    }
    let tokens = tokenize(rule)?;
    let (tokens, groups) = collection_groups(tokens)?;
    Ok((translate_tokens(&tokens)?, groups))
}

/// Takes `@collection` conditions out of the token stream. The first
/// condition of a group becomes its placeholder; later ones become `true`.
fn collection_groups(tokens: Vec<Token>) -> Result<(Vec<Token>, Vec<Group>), String> {
    let mut groups: Vec<Group> = Vec::new();
    let mut out = Vec::with_capacity(tokens.len());
    let n = tokens.len();
    let mut i = 0;
    while i < n {
        let t = &tokens[i];
        if i + 2 < n
            && t.is(Kind::Identifier)
            && t.value.starts_with("@collection.")
            && tokens[i + 1].is(Kind::Operator)
        {
            let right = &tokens[i + 2];
            if matches!(right.kind, Kind::Identifier | Kind::String | Kind::Number) {
                let rest = &t.value["@collection.".len()..];
                let Some((key, field)) = rest.split_once('.') else {
                    return Err(format!("invalid @collection identifier: {}", t.value));
                };
                let table = key.split_once(':').map_or(key, |(table, _)| table);
                let is_path = right.is(Kind::Identifier);
                let value = if is_path {
                    right
                        .value
                        .strip_prefix('@')
                        .unwrap_or(&right.value)
                        .to_owned()
                } else {
                    right.value.clone()
                };
                let condition = Condition {
                    field: field.to_owned(),
                    operator: tokens[i + 1].value.clone(),
                    value,
                    is_path,
                };
                let first = match groups.iter_mut().find(|g| g.key == key) {
                    Some(g) => {
                        g.conditions.push(condition);
                        false
                    }
                    None => {
                        groups.push(Group {
                            key: key.to_owned(),
                            table: table.to_owned(),
                            conditions: vec![condition],
                        });
                        true
                    }
                };
                let value = if first {
                    groups
                        .iter()
                        .find(|g| g.key == key)
                        .map(Group::placeholder)
                        .unwrap_or_default()
                } else {
                    "true".to_owned()
                };
                out.push(Token {
                    kind: Kind::Identifier,
                    value,
                });
                i += 3;
                continue;
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    Ok((out, groups))
}

/// An operand as the Go translator writes it: `@` dropped from identifiers,
/// strings quoted, anything else as is.
fn operand(t: &Token) -> String {
    match t.kind {
        Kind::Identifier => t.value.strip_prefix('@').unwrap_or(&t.value).to_owned(),
        Kind::String => go::quote(&t.value),
        _ => t.value.clone(),
    }
}

fn translate_tokens(tokens: &[Token]) -> Result<String, String> {
    let mut out = String::new();
    let n = tokens.len();
    let mut i = 0;
    while i < n {
        let t = &tokens[i];

        // `field:each <op> value`
        if i + 2 < n
            && t.is(Kind::Identifier)
            && t.value.ends_with(":each")
            && tokens[i + 1].is(Kind::Operator)
        {
            let base = t.value.strip_suffix(":each").unwrap_or(&t.value);
            let base = base.strip_prefix('@').unwrap_or(base);
            let op = &tokens[i + 1].value;
            let func = match op.as_str() {
                "=" | "==" => "all_eq",
                "!=" => "all_neq",
                "~" => "all_like",
                "!~" => "all_not_like",
                ">" => "all_gt",
                ">=" => "all_gte",
                "<" => "all_lt",
                "<=" => "all_lte",
                _ => return Err(format!("unsupported operator {op} with :each modifier")),
            };
            let value = &tokens[i + 2];
            let value = if value.is(Kind::String) {
                go::quote(&value.value)
            } else {
                value.value.clone()
            };
            out.push_str(&format!("{func}({base}, {value})"));
            i += 3;
            continue;
        }

        // Any-of operators: `?=`, `?~`, ...
        if i + 2 < n && tokens[i + 1].is(Kind::Operator) && tokens[i + 1].value.starts_with('?') {
            let left = if t.is(Kind::Identifier) || t.is(Kind::String) {
                operand(t)
            } else {
                t.value.clone()
            };
            let op = &tokens[i + 1].value;
            let right = &tokens[i + 2];
            let right = if right.is(Kind::Identifier) || right.is(Kind::String) {
                operand(right)
            } else {
                right.value.clone()
            };
            let func = match op.as_str() {
                "?=" => "any_eq",
                "?!=" => "any_neq",
                "?~" => "any_like",
                "?!~" => "any_not_like",
                "?>" => "any_gt",
                "?>=" => "any_gte",
                "?<" => "any_lt",
                "?<=" => "any_lte",
                _ => return Err(format!("unsupported wildcard operator {op}")),
            };
            out.push_str(&format!("{func}({left}, {right})"));
            i += 3;
            continue;
        }

        // `~` and `!~` look at the next token's text only, as in Go.
        if i + 2 < n && (tokens[i + 1].value == "~" || tokens[i + 1].value == "!~") {
            let left = if t.is(Kind::Identifier) || t.is(Kind::String) {
                operand(t)
            } else {
                t.value.clone()
            };
            let right = &tokens[i + 2];
            let right = if right.is(Kind::Identifier) || right.is(Kind::String) {
                operand(right)
            } else {
                right.value.clone()
            };
            let not = if tokens[i + 1].value == "~" { "" } else { "!" };
            out.push_str(&format!("{not}like({left}, {right})"));
            i += 3;
            continue;
        }

        match t.kind {
            Kind::Operator => {
                let op = if t.value == "=" { "==" } else { &t.value };
                out.push(' ');
                out.push_str(op);
                out.push(' ');
            }
            Kind::Identifier => {
                let value = t.value.strip_prefix('@').unwrap_or(&t.value);
                if value.contains(':') {
                    let mut parts = value.split(':');
                    let base = parts.next().unwrap_or_default();
                    let base = base.strip_prefix('@').unwrap_or(base);
                    let modifier = parts.next().unwrap_or_default();
                    match modifier {
                        "lower" => out.push_str(&format!("lower({base})")),
                        "length" => out.push_str(&format!("length({base})")),
                        "isset" => {
                            let Some((parent, key)) = base.rsplit_once('.') else {
                                return Err(format!("invalid path for :isset modifier: {value}"));
                            };
                            out.push_str(&format!("isset({parent}, {})", go::quote(key)));
                        }
                        "changed" => {
                            let Some((_, key)) = base.rsplit_once('.') else {
                                return Err(format!("invalid path for :changed modifier: {value}"));
                            };
                            out.push_str(&format!(
                                "changed(_record, {}, request.body)",
                                go::quote(key)
                            ));
                        }
                        _ => return Err(format!("unsupported modifier :{modifier}")),
                    }
                } else if t.value.starts_with('@') && is_date_macro(value) {
                    out.push('_');
                    out.push_str(value);
                } else {
                    out.push_str(value);
                }
            }
            Kind::String => out.push_str(&go::quote(&t.value)),
            Kind::Logical => {
                let op = t.value.to_ascii_lowercase();
                let op = match op.as_str() {
                    "and" => "&&",
                    "or" => "||",
                    other => other,
                };
                out.push(' ');
                out.push_str(op);
                out.push(' ');
            }
            Kind::Number | Kind::Paren | Kind::Comma => out.push_str(&t.value),
        }
        i += 1;
    }
    Ok(out)
}

/// The names `@now`, `@todayStart`, ... that read the clock.
pub(crate) fn is_date_macro(name: &str) -> bool {
    matches!(
        name,
        "now"
            | "yesterday"
            | "tomorrow"
            | "todayStart"
            | "todayEnd"
            | "monthStart"
            | "monthEnd"
            | "yearStart"
            | "yearEnd"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(rule: &str) -> String {
        translate(rule).unwrap().0
    }

    #[test]
    fn translates_like_go() {
        assert_eq!(tr("status = 'active'"), r#"status == "active""#);
        assert_eq!(tr("@request.auth.id != ''"), r#"request.auth.id != """#);
        assert_eq!(tr("title ~ 'Lorem%'"), r#"like(title, "Lorem%")"#);
        assert_eq!(tr("title !~ 'x'"), r#"!like(title, "x")"#);
        assert_eq!(tr("tags ?= 'test'"), r#"any_eq(tags, "test")"#);
        assert_eq!(tr("tags:each ~ 'tag'"), r#"all_like(tags, "tag")"#);
        assert_eq!(tr("title:lower = 'x'"), r#"lower(title) == "x""#);
        assert_eq!(
            tr("@request.body.role:isset = false"),
            r#"isset(request.body, "role") == false"#
        );
        assert_eq!(
            tr("@request.body.role:changed = false"),
            r#"changed(_record, "role", request.body) == false"#
        );
        assert_eq!(
            tr("createdAt < @now and a = 1 OR b = 2"),
            "createdAt < _now && a == 1 || b == 2"
        );
        assert_eq!(tr("invalid == (&&"), "invalid == ( && ");
        assert_eq!(tr("geoDistance(1, 2) < 5"), "geoDistance(1,2) < 5");
    }

    #[test]
    fn groups_collection_conditions() {
        let (expr, groups) =
            translate("@collection.user_roles.user_id = @request.auth.id && @collection.user_roles.role = 'admin'")
                .unwrap();
        assert_eq!(expr, "exists_group_user_roles && true");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].conditions[0].value, "request.auth.id");
        assert!(groups[0].conditions[0].is_path);
        assert_eq!(groups[0].conditions[1].value, "admin");
    }

    #[test]
    fn reports_errors_like_go() {
        assert_eq!(
            translate("a:upper = 1").unwrap_err(),
            "unsupported modifier :upper"
        );
        assert_eq!(
            translate("role:isset = 1").unwrap_err(),
            "invalid path for :isset modifier: role:isset"
        );
        assert_eq!(
            translate("tags:each ?= 1").unwrap_err(),
            "unsupported operator ?= with :each modifier"
        );
        assert_eq!(
            translate("@collection.x = 1").unwrap_err(),
            "invalid @collection identifier: @collection.x"
        );
    }
}
