#![doc = include_str!("../README.md")]

use std::fmt;

/// `?` for the recursive functions of the parser and the evaluator. A debug
/// build gives each `?` several temporaries in the frame, and these frames
/// stack up once per nesting level; a plain `match` needs fewer.
macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return Err(e),
        }
    };
}

mod evaluate;
mod expr;
mod go;
mod sql;
mod tokenize;
mod translate;

pub use evaluate::{EvalContext, Lookup, check, evaluate};
pub use sql::{
    FilterOptions, LoadCollection, SortOptions, SqlFilter, filter_sql, is_secret_field,
    is_valid_field, owner_fields, sort_sql,
};

/// A JSON object: a record, the caller, or the request.
pub type Map = serde_json::Map<String, serde_json::Value>;

/// How deeply a filter or rule may nest. A filter counts its parentheses.
/// A rule counts the expressions inside expressions: each parenthesis, call
/// argument and `not`, and the right side of each operator, so `a = 1 && (b
/// = 2)` is 4 deep. Deeper input is refused with an error before it can
/// exhaust the stack, which is 1 MiB in a browser worker; the deepest input
/// allowed runs on 256 KiB even in a debug build. The Go engine had no such
/// limit: its stacks grow.
pub const MAX_DEPTH: usize = 32;

/// A rule that cannot be read, compiled or run. The message matches the Go
/// engine's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
        }
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Go's `rules.Translate`: the expr-lang expression a rule stands for.
/// The Go engine showed it in its rule tester; it is how `evaluate` reads
/// a rule.
pub fn translate(rule: &str) -> Result<String, Error> {
    translate::translate(rule)
        .map(|(expr, _)| expr)
        .map_err(Error::new)
}
