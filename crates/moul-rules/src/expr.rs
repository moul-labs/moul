//! The part of expr-lang (github.com/expr-lang/expr v1.17) the Go engine's
//! translated rules use: its lexer and parser, with the same precedence and
//! the same error messages and positions.
//!
//! Constructs a translated rule cannot produce (arrays, maps, closures,
//! `let`, `if`, pipes, slices) are refused with an error of their own.

use crate::go;

/// A lexing, parsing, checking or running error, with the character
/// position it points at.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExprError {
    pub message: String,
    /// Character index into the expression. `None` prints the message alone.
    pub at: Option<usize>,
}

impl ExprError {
    pub fn at(at: usize, message: impl Into<String>) -> ExprError {
        ExprError {
            message: message.into(),
            at: Some(at),
        }
    }

    pub fn plain(message: impl Into<String>) -> ExprError {
        ExprError {
            message: message.into(),
            at: None,
        }
    }

    /// The message as expr-lang prints it: `msg (line:col)`, the source line
    /// and a caret under the position.
    pub fn render(&self, src: &str) -> String {
        let Some(at) = self.at else {
            return self.message.clone();
        };
        let mut line = 1;
        let mut column = 0;
        let mut line_start = 0;
        for (count, (i, c)) in src.char_indices().enumerate() {
            if count == at {
                break;
            }
            if c == '\n' {
                line_start = i + 1;
                line += 1;
                column = 0;
            } else {
                column += 1;
            }
        }
        let line_end = src[line_start..]
            .find('\n')
            .map_or(src.len(), |i| line_start + i);
        if line_start == line_end {
            return self.message.clone();
        }
        let text = src[line_start..line_end].replace('\t', " ");
        format!(
            "{} ({}:{})\n | {}\n | {}^",
            self.message,
            line,
            column + 1,
            text,
            ".".repeat(column)
        )
    }
}

// ---------------------------------------------------------------- lexer

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TKind {
    Identifier,
    Number,
    String,
    Bytes,
    Operator,
    Bracket,
    Eof,
}

impl TKind {
    fn name(self) -> &'static str {
        match self {
            TKind::Identifier => "Identifier",
            TKind::Number => "Number",
            TKind::String => "String",
            TKind::Bytes => "Bytes",
            TKind::Operator => "Operator",
            TKind::Bracket => "Bracket",
            TKind::Eof => "EOF",
        }
    }
}

#[derive(Clone, Debug)]
struct Tok {
    kind: TKind,
    value: String,
    at: usize,
}

impl Tok {
    fn is(&self, kind: TKind, value: &str) -> bool {
        self.kind == kind && self.value == value
    }

    /// How expr-lang names a token in messages: `Operator("&&")`.
    fn describe(&self) -> String {
        if self.value.is_empty() {
            self.kind.name().to_owned()
        } else {
            format!("{}({})", self.kind.name(), go::quote(&self.value))
        }
    }
}

fn is_alphabetic(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphabetic()
}

fn is_alphanumeric(c: char) -> bool {
    is_alphabetic(c) || c.is_numeric()
}

struct Lexer {
    chars: Vec<char>,
    start: usize,
    end: usize,
    eof: bool,
    tokens: Vec<Tok>,
    err: Option<ExprError>,
}

impl Lexer {
    fn next(&mut self) -> Option<char> {
        if self.end >= self.chars.len() {
            self.eof = true;
            return None;
        }
        self.end += 1;
        Some(self.chars[self.end - 1])
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.end).copied()
    }

    fn backup(&mut self) {
        if self.eof {
            self.eof = false;
        } else if self.end > 0 {
            self.end -= 1;
        }
    }

    fn word(&self) -> String {
        self.chars[self.start..self.end].iter().collect()
    }

    fn emit(&mut self, kind: TKind) {
        let value = self.word();
        self.emit_value(kind, value);
    }

    fn emit_value(&mut self, kind: TKind, value: String) {
        self.tokens.push(Tok {
            kind,
            value,
            at: self.start,
        });
        self.start = self.end;
    }

    fn skip(&mut self) {
        self.start = self.end;
    }

    fn accept(&mut self, valid: &str) -> bool {
        match self.peek() {
            Some(c) if valid.contains(c) => {
                self.next();
                true
            }
            _ => false,
        }
    }

    fn accept_run(&mut self, valid: &str) {
        while self.accept(valid) {}
    }

    fn error(&mut self, message: impl Into<String>) {
        if self.err.is_none() {
            let end = if self.eof { self.end + 1 } else { self.end };
            self.err = Some(ExprError::at(end.saturating_sub(1), message));
        }
    }

    fn run(&mut self) {
        while self.err.is_none() {
            let Some(r) = self.next() else {
                let at = self.end.saturating_sub(1);
                self.tokens.push(Tok {
                    kind: TKind::Eof,
                    value: String::new(),
                    at,
                });
                return;
            };
            match r {
                _ if r.is_whitespace() => self.skip(),
                '\'' | '"' => {
                    self.scan_string(r);
                    if self.err.is_some() {
                        return;
                    }
                    match unescape(&self.word()) {
                        Ok(s) => self.emit_value(TKind::String, s),
                        Err(e) => self.error(e),
                    }
                }
                '`' => self.scan_raw_string(),
                'b' | 'B' if matches!(self.peek(), Some('\'' | '"')) => {
                    let quote = self.next().unwrap_or('"');
                    self.scan_string(quote);
                    if self.err.is_some() {
                        return;
                    }
                    let word = self.word();
                    match unescape(&word[1..]) {
                        Ok(s) => self.emit_value(TKind::Bytes, s),
                        Err(e) => self.error(e),
                    }
                }
                '0'..='9' => {
                    self.backup();
                    self.number();
                }
                '?' => {
                    self.accept(".?");
                    self.emit(TKind::Operator);
                }
                '/' => {
                    if self.accept("/") {
                        while !matches!(self.next(), None | Some('\n')) {}
                        self.skip();
                    } else if self.accept("*") {
                        loop {
                            match self.next() {
                                None => return self.error("unclosed comment"),
                                Some('*') if self.accept("/") => break,
                                _ => {}
                            }
                        }
                        self.skip();
                    } else {
                        self.emit(TKind::Operator);
                    }
                }
                '#' => {
                    self.accept("#");
                    self.emit(TKind::Operator);
                    while self.peek().is_some_and(is_alphanumeric) {
                        self.next();
                    }
                    if self.end > self.start {
                        self.emit(TKind::Identifier);
                    }
                }
                '|' => {
                    self.accept("|");
                    self.emit(TKind::Operator);
                }
                ':' => {
                    self.accept(":");
                    self.emit(TKind::Operator);
                }
                '(' | '[' | '{' | ')' | ']' | '}' => self.emit(TKind::Bracket),
                ',' | ';' | '%' | '+' | '-' | '^' => self.emit(TKind::Operator),
                '&' | '!' | '=' | '*' | '<' | '>' => {
                    self.accept("&=*");
                    self.emit(TKind::Operator);
                }
                '.' => {
                    // `.5` is a number; otherwise `.` or `..`.
                    if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.number();
                    } else {
                        self.accept(".");
                        self.emit(TKind::Operator);
                    }
                }
                _ if is_alphanumeric(r) => {
                    self.backup();
                    self.identifier();
                }
                _ => return self.error(format!("unrecognized character: {}", go::rune_u(r))),
            }
        }
    }

    fn number(&mut self) {
        if !self.scan_number() {
            let word = self.word();
            return self.error(format!("bad number syntax: {}", go::quote(&word)));
        }
        self.emit(TKind::Number);
    }

    fn scan_number(&mut self) -> bool {
        let mut digits = "0123456789_";
        if self.accept("0") {
            if self.accept("xX") {
                digits = "0123456789abcdefABCDEF_";
            } else if self.accept("oO") {
                digits = "01234567_";
            } else if self.accept("bB") {
                digits = "01_";
            }
        }
        self.accept_run(digits);
        let end = self.end;
        if self.accept(".") {
            if self.peek() == Some('.') {
                self.end = end;
                return true;
            }
            self.accept_run(digits);
        }
        if self.accept("eE") {
            self.accept("+-");
            self.accept_run(digits);
        }
        if self.peek().is_some_and(is_alphanumeric) {
            self.next();
            return false;
        }
        true
    }

    fn identifier(&mut self) {
        while self.peek().is_some_and(is_alphanumeric) {
            self.next();
        }
        match self.word().as_str() {
            "not" => {
                self.emit(TKind::Operator);
                self.accept_run(" ");
                self.skip();
                let end = self.end;
                while self.peek().is_some_and(is_alphanumeric) {
                    self.next();
                }
                match self.word().as_str() {
                    "in" | "matches" | "contains" | "startsWith" | "endsWith" => {
                        self.emit(TKind::Operator)
                    }
                    _ => self.end = end,
                }
            }
            "in" | "or" | "and" | "matches" | "contains" | "startsWith" | "endsWith" | "let"
            | "if" | "else" => self.emit(TKind::Operator),
            _ => self.emit(TKind::Identifier),
        }
    }

    fn scan_string(&mut self, quote: char) {
        let mut ch = self.next();
        while ch != Some(quote) {
            match ch {
                None | Some('\n') => return self.error("literal not terminated"),
                Some('\\') => ch = self.scan_escape(quote),
                _ => ch = self.next(),
            }
            if self.err.is_some() {
                return;
            }
        }
    }

    fn scan_escape(&mut self, quote: char) -> Option<char> {
        let ch = self.next();
        match ch {
            Some(c) if "abfnrtv\\".contains(c) || c == quote => self.next(),
            Some('0'..='7') => self.scan_digits(ch, 8, 3),
            Some('x') => {
                let c = self.next();
                self.scan_digits(c, 16, 2)
            }
            Some('u') if self.peek() == Some('{') => {
                self.next();
                let mut digits = 0;
                while self.peek() != Some('}') {
                    if !self.peek().is_some_and(|c| c.is_ascii_hexdigit()) || digits >= 6 {
                        self.error("invalid char escape");
                        return None;
                    }
                    self.next();
                    digits += 1;
                }
                if digits == 0 {
                    self.error("invalid char escape");
                    return None;
                }
                self.next();
                self.next()
            }
            Some('u') => {
                let c = self.next();
                self.scan_digits(c, 16, 4)
            }
            Some('U') => {
                let c = self.next();
                self.scan_digits(c, 16, 8)
            }
            _ => {
                self.error("invalid char escape");
                ch
            }
        }
    }

    fn scan_digits(&mut self, mut ch: Option<char>, base: u32, mut n: usize) -> Option<char> {
        while n > 0 && ch.is_some_and(|c| c.is_digit(base)) {
            ch = self.next();
            n -= 1;
        }
        if n > 0 {
            self.error("invalid char escape");
        }
        ch
    }

    fn scan_raw_string(&mut self) {
        loop {
            match self.next() {
                Some('`') if self.peek() == Some('`') => {
                    self.next();
                }
                Some('`') => break,
                None => return self.error("literal not terminated"),
                _ => {}
            }
        }
        let word = self.word();
        let inner = &word[1..word.len() - 1];
        self.emit_value(TKind::String, inner.replace("``", "`"));
    }
}

/// expr-lang's `unescape`: the value of a quoted string literal.
fn unescape(quoted: &str) -> Result<String, String> {
    let quoted = quoted.replace("\r\n", "\n").replace('\r', "\n");
    let fail = || "unable to unescape string".to_owned();
    let mut chars = quoted.chars();
    let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else {
        return Err(fail());
    };
    if first != last || (first != '"' && first != '\'') {
        return Err(fail());
    }
    let body: Vec<char> = chars.collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    let hex = |s: &[char]| -> Option<u32> {
        let text: String = s.iter().collect();
        u32::from_str_radix(&text, 16).ok()
    };
    while i < body.len() {
        let c = body[i];
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let Some(&e) = body.get(i + 1) else {
            return Err("unable to unescape string, found '\\' as last character".into());
        };
        i += 2;
        let value = match e {
            'a' => '\u{7}',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{b}',
            '\\' | '\'' | '"' | '`' | '?' => e,
            'u' if body.get(i) == Some(&'{') => {
                let close = body[i..].iter().position(|c| *c == '}').ok_or_else(fail)?;
                let v = hex(&body[i + 1..i + close]).ok_or_else(fail)?;
                i += close + 1;
                char::from_u32(v).ok_or_else(fail)?
            }
            'x' | 'X' | 'u' | 'U' => {
                let n = match e {
                    'x' | 'X' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let digits = body.get(i..i + n).ok_or_else(fail)?;
                let v = hex(digits).ok_or_else(fail)?;
                i += n;
                char::from_u32(v).ok_or_else(fail)?
            }
            '0'..='3' => {
                let digits: String = body.get(i - 1..i + 2).ok_or_else(fail)?.iter().collect();
                let v = u32::from_str_radix(&digits, 8).map_err(|_| fail())?;
                i += 2;
                char::from_u32(v).ok_or_else(fail)?
            }
            _ => return Err(fail()),
        };
        out.push(value);
    }
    Ok(out)
}

fn lex(src: &str) -> (Vec<Tok>, Option<ExprError>) {
    let mut lexer = Lexer {
        chars: src.chars().collect(),
        start: 0,
        end: 0,
        eof: false,
        tokens: Vec::new(),
        err: None,
    };
    lexer.run();
    (lexer.tokens, lexer.err)
}

// ---------------------------------------------------------------- AST

/// A parsed expression. `at` is where expr-lang locates the node: the
/// operator of an operation, the name of a call, the property of a member.
///
/// A chain such as `a && b && c` or `a.b.c` grows through first children
/// (see [`Node::first`]) without the parser nesting, so it can be thousands
/// of nodes deep. Code that walks a tree follows first children in a loop
/// and recurses only into the others, whose depth the parser bounds.
#[derive(Debug)]
pub(crate) struct Node {
    pub kind: NodeKind,
    pub at: usize,
}

#[derive(Clone, Debug)]
pub(crate) enum NodeKind {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),
    /// `base.prop`. `method` when it is called. `chain` when expr-lang
    /// wraps it in a chain node: it is optional or follows an optional one.
    Member {
        base: Box<Node>,
        prop: String,
        optional: bool,
        method: bool,
        chain: bool,
    },
    Unary {
        op: &'static str,
        node: Box<Node>,
    },
    Binary {
        op: &'static str,
        left: Box<Node>,
        right: Box<Node>,
    },
    Call {
        callee: Box<Node>,
        args: Vec<Node>,
    },
    Cond {
        cond: Box<Node>,
        yes: Box<Node>,
        no: Box<Node>,
    },
}

impl Node {
    /// A leaf that stands in for a node moved out of the tree.
    pub fn placeholder() -> Node {
        Node {
            kind: NodeKind::Nil,
            at: 0,
        }
    }

    /// The child a walk visits first, the one a chain grows through: the
    /// base of a member, the operand of a unary operator, the left side of
    /// a binary one, the condition of `?:` and the callee of a call. A leaf
    /// has none.
    pub fn first(&self) -> Option<&Node> {
        match &self.kind {
            NodeKind::Member { base: n, .. }
            | NodeKind::Unary { node: n, .. }
            | NodeKind::Binary { left: n, .. }
            | NodeKind::Cond { cond: n, .. }
            | NodeKind::Call { callee: n, .. } => Some(n),
            _ => None,
        }
    }

    /// [`Node::first`], to change it.
    pub fn first_mut(&mut self) -> Option<&mut Node> {
        match &mut self.kind {
            NodeKind::Member { base: n, .. }
            | NodeKind::Unary { node: n, .. }
            | NodeKind::Binary { left: n, .. }
            | NodeKind::Cond { cond: n, .. }
            | NodeKind::Call { callee: n, .. } => Some(n),
            _ => None,
        }
    }

    /// The nodes from this one down through first children: those that
    /// have a first child, top first, and the leaf at the bottom.
    pub fn spine(&self) -> (Vec<&Node>, &Node) {
        let mut parents = Vec::new();
        let mut node = self;
        while let Some(first) = node.first() {
            parents.push(node);
            node = first;
        }
        (parents, node)
    }

    /// A copy of this node with `first` as its first child and copies of
    /// its other children. A leaf is copied as is.
    fn with_first(&self, first: Node) -> Node {
        let first = Box::new(first);
        let kind = match &self.kind {
            NodeKind::Member {
                prop,
                optional,
                method,
                chain,
                ..
            } => NodeKind::Member {
                base: first,
                prop: prop.clone(),
                optional: *optional,
                method: *method,
                chain: *chain,
            },
            NodeKind::Unary { op, .. } => NodeKind::Unary { op, node: first },
            NodeKind::Binary { op, right, .. } => NodeKind::Binary {
                op,
                left: first,
                right: right.clone(),
            },
            NodeKind::Cond { yes, no, .. } => NodeKind::Cond {
                cond: first,
                yes: yes.clone(),
                no: no.clone(),
            },
            NodeKind::Call { args, .. } => NodeKind::Call {
                callee: first,
                args: args.clone(),
            },
            leaf => leaf.clone(),
        };
        Node { kind, at: self.at }
    }
}

impl Clone for Node {
    /// Copies the first children in a loop, so a long chain needs no deep
    /// recursion.
    fn clone(&self) -> Node {
        let (parents, leaf) = self.spine();
        let mut node = Node {
            kind: leaf.kind.clone(),
            at: leaf.at,
        };
        for parent in parents.into_iter().rev() {
            node = parent.with_first(node);
        }
        node
    }
}

impl Drop for Node {
    /// Moves the children out before they drop, so a long chain needs no
    /// deep recursion: each node drops with leaves for children.
    fn drop(&mut self) {
        let mut stack = Vec::new();
        self.kind.take_children(&mut stack);
        while let Some(mut node) = stack.pop() {
            node.kind.take_children(&mut stack);
        }
    }
}

impl NodeKind {
    /// Moves the children that have children of their own into `out`,
    /// leaving placeholders.
    fn take_children(&mut self, out: &mut Vec<Node>) {
        fn take(node: &mut Node, out: &mut Vec<Node>) {
            if node.first().is_some() {
                out.push(std::mem::replace(node, Node::placeholder()));
            }
        }
        match self {
            NodeKind::Member { base: n, .. } | NodeKind::Unary { node: n, .. } => take(n, out),
            NodeKind::Binary { left, right, .. } => {
                take(left, out);
                take(right, out);
            }
            NodeKind::Cond { cond, yes, no } => {
                take(cond, out);
                take(yes, out);
                take(no, out);
            }
            NodeKind::Call { callee, args } => {
                take(callee, out);
                for arg in args {
                    take(arg, out);
                }
            }
            _ => {}
        }
    }
}

/// Whether expr-lang would have wrapped `node` in a chain node, so that a
/// member access after it is wrapped too.
fn is_chain(node: &Node) -> bool {
    match &node.kind {
        NodeKind::Member { chain, .. } => *chain,
        NodeKind::Call { callee, .. } => {
            matches!(callee.kind, NodeKind::Member { chain: true, .. })
        }
        _ => false,
    }
}

/// Binary operators: name, precedence, right-associative. `and` and `or`
/// keep their spelling for messages; they mean `&&` and `||`.
fn binary(op: &str) -> Option<(&'static str, u16, bool)> {
    Some(match op {
        "|" => ("|", 0, false),
        "or" => ("or", 10, false),
        "||" => ("||", 10, false),
        "and" => ("and", 15, false),
        "&&" => ("&&", 15, false),
        "==" => ("==", 20, false),
        "!=" => ("!=", 20, false),
        "<" => ("<", 20, false),
        ">" => (">", 20, false),
        ">=" => (">=", 20, false),
        "<=" => ("<=", 20, false),
        "in" => ("in", 20, false),
        "matches" => ("matches", 20, false),
        "contains" => ("contains", 20, false),
        "startsWith" => ("startsWith", 20, false),
        "endsWith" => ("endsWith", 20, false),
        ".." => ("..", 25, false),
        "+" => ("+", 30, false),
        "-" => ("-", 30, false),
        "*" => ("*", 60, false),
        "/" => ("/", 60, false),
        "%" => ("%", 60, false),
        "**" => ("**", 100, true),
        "^" => ("^", 100, true),
        "??" => ("??", 500, false),
        _ => return None,
    })
}

fn unary(op: &str) -> Option<(&'static str, u16)> {
    Some(match op {
        "not" => ("not", 50),
        "!" => ("!", 50),
        "-" => ("-", 90),
        "+" => ("+", 90),
        _ => return None,
    })
}

fn is_comparison(op: &str) -> bool {
    matches!(op, "<" | ">" | ">=" | "<=")
}

const PREDICATES: &[&str] = &[
    "all",
    "none",
    "any",
    "one",
    "filter",
    "map",
    "count",
    "sum",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "groupBy",
    "sortBy",
    "reduce",
];

// ---------------------------------------------------------------- parser

/// expr-lang's `conf.DefaultMaxNodes`: an expression of more nodes is
/// refused, as the Go engine refused it.
const MAX_NODES: usize = 10_000;

/// The error expr-lang gives past `MAX_NODES`.
const TOO_MANY_NODES: &str = "compilation failed: expression exceeds maximum allowed nodes";

/// The error past `MAX_DEPTH` nested expressions. expr-lang has no such
/// limit.
const TOO_DEEP: &str = "expression is nested too deeply";

struct Parser {
    tokens: std::vec::IntoIter<Tok>,
    lex_err: Option<ExprError>,
    current: Tok,
    stashed: Option<Tok>,
    /// How many `expression` calls are open. Every recursion of the parser
    /// goes through one.
    depth: usize,
    /// How many nodes expr-lang would have made so far.
    nodes: usize,
}

type Parsed = Result<Node, ExprError>;

/// Parses an expression the way expr-lang does. More than `MAX_NODES`
/// nodes, or more than `MAX_DEPTH` nested expressions, are refused.
pub(crate) fn parse(src: &str) -> Parsed {
    let (tokens, lex_err) = lex(src);
    let mut p = Parser {
        tokens: tokens.into_iter(),
        lex_err,
        current: Tok {
            kind: TKind::Eof,
            value: String::new(),
            at: 0,
        },
        stashed: None,
        depth: 0,
        nodes: 0,
    };
    p.next()?;
    let node = p.expression(0)?;
    if p.current.is(TKind::Operator, ";") {
        return Err(p.unsupported());
    }
    if p.current.kind != TKind::Eof {
        return Err(p.unexpected());
    }
    Ok(node)
}

impl Parser {
    fn next(&mut self) -> Result<(), ExprError> {
        if let Some(t) = self.stashed.take() {
            self.current = t;
            return Ok(());
        }
        match self.tokens.next() {
            Some(t) => {
                self.current = t;
                Ok(())
            }
            None => Err(self
                .lex_err
                .take()
                .unwrap_or_else(|| ExprError::at(self.current.at, "unexpected end of expression"))),
        }
    }

    fn unexpected(&self) -> ExprError {
        ExprError::at(
            self.current.at,
            format!("unexpected token {}", self.current.describe()),
        )
    }

    fn unsupported(&self) -> ExprError {
        ExprError::at(
            self.current.at,
            format!("{} is not supported in rules", self.current.describe()),
        )
    }

    fn expect(&mut self, kind: TKind, value: &str) -> Result<(), ExprError> {
        if self.current.is(kind, value) {
            self.next()
        } else {
            Err(self.unexpected())
        }
    }

    fn node(kind: NodeKind, at: usize) -> Node {
        Node { kind, at }
    }

    /// Counts a node where expr-lang creates one, and fails as it does at
    /// the current token once there are more than `MAX_NODES`.
    fn count(&mut self) -> Result<(), ExprError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ExprError::at(self.current.at, TOO_MANY_NODES));
        }
        Ok(())
    }

    // Every recursion of the parser goes through `expression`, and a rule
    // may nest it `MAX_DEPTH` deep, so the functions on the way keep their
    // frames small: they hand anything that needs no recursion to helpers,
    // whose frames are gone before the next level starts.

    /// An expression whose operators bind at least as tightly as
    /// `precedence`. Past `MAX_DEPTH` nested ones it fails, before deep
    /// input can exhaust the stack.
    fn expression(&mut self, precedence: u16) -> Parsed {
        if self.depth >= crate::MAX_DEPTH {
            return Err(ExprError::at(self.current.at, TOO_DEEP));
        }
        self.depth += 1;
        let node = self.operation(precedence);
        self.depth -= 1;
        node
    }

    fn operation(&mut self, precedence: u16) -> Parsed {
        if precedence == 0 {
            tri!(self.refuse_statement());
        }
        let mut left = tri!(self.primary());
        let mut prev = "";
        while let Some(op) = tri!(self.operator(precedence, prev)) {
            prev = op.op;
            left = tri!(self.right_side(left, op));
        }
        if precedence == 0 && self.current.is(TKind::Operator, "?") {
            return self.conditional(left);
        }
        Ok(left)
    }

    /// The operation of `op` on `left` and the operand after `op`.
    fn right_side(&mut self, left: Node, op: Operator) -> Parsed {
        if is_comparison(op.op) {
            return self.comparison(left, op.op, op.at, op.precedence);
        }
        let right = tri!(self.expression(if op.right_assoc {
            op.precedence
        } else {
            op.precedence + 1
        }));
        self.binary_node(left, right, &op)
    }

    /// `let` and `if` start statements, which rules do not have.
    fn refuse_statement(&self) -> Result<(), ExprError> {
        if self.current.is(TKind::Operator, "let") || self.current.is(TKind::Operator, "if") {
            return Err(self.unsupported());
        }
        Ok(())
    }

    /// The binary operator at the current token, consumed, if `operation`
    /// takes it: one that binds at least as tightly as `precedence`. `not
    /// in` (and `not contains`, ...) is one operator, negated. `prev` is the
    /// operator taken before it.
    fn operator(&mut self, precedence: u16, prev: &str) -> Result<Option<Operator>, ExprError> {
        if self.current.kind != TKind::Operator {
            return Ok(None);
        }
        let mut not_at = None;
        if self.current.value == "not" {
            let backup = self.current.clone();
            tri!(self.next());
            if !matches!(
                self.current.value.as_str(),
                "contains" | "matches" | "startsWith" | "endsWith" | "in"
            ) {
                return Err(self.unexpected());
            }
            match binary(&self.current.value) {
                Some((_, p, _)) if p >= precedence => not_at = Some(self.current.at),
                _ => {
                    self.stashed = Some(std::mem::replace(&mut self.current, backup));
                    return Ok(None);
                }
            }
        }
        let Some((op, p, right_assoc)) = binary(&self.current.value) else {
            return Ok(None);
        };
        if p < precedence {
            return Ok(None);
        }
        let at = self.current.at;
        tri!(self.next());
        if op == "|" {
            return Err(ExprError::at(
                at,
                "the pipe operator is not supported in rules",
            ));
        }
        if prev == "??" && op != "??" {
            return Err(ExprError::at(
                at,
                format!(
                    "Operator ({op}) and coalesce expressions (??) cannot be mixed. Wrap either by parentheses."
                ),
            ));
        }
        Ok(Some(Operator {
            op,
            at,
            precedence: p,
            right_assoc,
            not_at,
        }))
    }

    /// The node of a binary operation, under a `not` for `not in` and the
    /// like.
    fn binary_node(&mut self, left: Node, right: Node, op: &Operator) -> Parsed {
        tri!(self.count());
        let mut node = Self::node(
            NodeKind::Binary {
                op: op.op,
                left: Box::new(left),
                right: Box::new(right),
            },
            op.at,
        );
        if let Some(not_at) = op.not_at {
            tri!(self.count());
            node = Self::node(
                NodeKind::Unary {
                    op: "not",
                    node: Box::new(node),
                },
                not_at,
            );
        }
        Ok(node)
    }

    fn conditional(&mut self, mut node: Node) -> Parsed {
        while self.current.is(TKind::Operator, "?") {
            tri!(self.next());
            let (yes, no);
            if !self.current.is(TKind::Operator, ":") {
                yes = tri!(self.expression(0));
                tri!(self.expect(TKind::Operator, ":"));
                no = tri!(self.expression(0));
            } else {
                tri!(self.next());
                yes = node.clone();
                no = tri!(self.expression(0));
            }
            node = tri!(self.conditional_node(node, yes, no));
        }
        Ok(node)
    }

    fn conditional_node(&mut self, cond: Node, yes: Node, no: Node) -> Parsed {
        tri!(self.count());
        Ok(Self::node(
            NodeKind::Cond {
                cond: Box::new(cond),
                yes: Box::new(yes),
                no: Box::new(no),
            },
            self.current.at,
        ))
    }

    fn primary(&mut self) -> Parsed {
        if self.current.kind == TKind::Operator
            && let Some((op, p)) = unary(&self.current.value)
        {
            return self.unary_operation(op, p);
        }
        if self.current.is(TKind::Bracket, "(") {
            return self.parenthesized();
        }
        if self.current.is(TKind::Operator, "::") {
            return Err(self.unsupported());
        }
        self.secondary()
    }

    /// A unary operator at the current token and its operand.
    fn unary_operation(&mut self, op: &'static str, precedence: u16) -> Parsed {
        let at = self.current.at;
        tri!(self.next());
        let operand = tri!(self.expression(precedence));
        let node = tri!(self.unary_node(op, operand, at));
        self.postfix(node)
    }

    fn unary_node(&mut self, op: &'static str, operand: Node, at: usize) -> Parsed {
        tri!(self.count());
        Ok(Self::node(
            NodeKind::Unary {
                op,
                node: Box::new(operand),
            },
            at,
        ))
    }

    /// An expression in parentheses, from the `(` at the current token.
    fn parenthesized(&mut self) -> Parsed {
        tri!(self.next());
        let node = tri!(self.expression(0));
        tri!(self.close_parenthesis());
        self.postfix(node)
    }

    fn close_parenthesis(&mut self) -> Result<(), ExprError> {
        if self.current.is(TKind::Operator, ";") {
            return Err(self.unsupported());
        }
        self.expect(TKind::Bracket, ")")
    }

    fn secondary(&mut self) -> Parsed {
        match self.current.kind {
            TKind::Identifier => self.identifier(),
            TKind::Number | TKind::String => self.literal(),
            TKind::Bytes => Err(ExprError::at(
                self.current.at,
                "byte strings are not supported in rules",
            )),
            _ if self.current.is(TKind::Bracket, "[") || self.current.is(TKind::Bracket, "{") => {
                Err(self.unsupported())
            }
            _ => Err(self.unexpected()),
        }
    }

    /// The current token, consumed.
    fn take(&mut self) -> Result<Tok, ExprError> {
        let tok = self.current.clone();
        tri!(self.next());
        Ok(tok)
    }

    /// A name, `true`, `false`, `nil` or a call, from the current token.
    fn identifier(&mut self) -> Parsed {
        let tok = tri!(self.take());
        if self.current.is(TKind::Bracket, "(")
            && !matches!(tok.value.as_str(), "true" | "false" | "nil")
        {
            return self.call(tok);
        }
        self.name(tok)
    }

    /// The name, `true`, `false` or `nil` in `tok`, already consumed.
    fn name(&mut self, tok: Tok) -> Parsed {
        tri!(self.count());
        let node = match tok.value.as_str() {
            "true" => return Ok(Self::node(NodeKind::Bool(true), tok.at)),
            "false" => return Ok(Self::node(NodeKind::Bool(false), tok.at)),
            "nil" => return Ok(Self::node(NodeKind::Nil, tok.at)),
            _ => Self::node(NodeKind::Ident(tok.value), tok.at),
        };
        self.postfix(node)
    }

    /// A number or a string, from the current token. A number takes no
    /// postfix.
    fn literal(&mut self) -> Parsed {
        let tok = tri!(self.take());
        if tok.kind == TKind::Number {
            let node = tri!(number(&tok));
            tri!(self.count());
            return Ok(node);
        }
        tri!(self.count());
        let node = Self::node(NodeKind::Str(tok.value), tok.at);
        self.postfix(node)
    }

    /// A call of the function `name`, from its `(` at the current token,
    /// and what follows it.
    fn call(&mut self, name: Tok) -> Parsed {
        tri!(self.callee(&name));
        let args = tri!(self.arguments());
        let node = tri!(self.call_node(name, args));
        self.postfix(node)
    }

    /// Refuses expr-lang's predicates and counts the callee's node.
    fn callee(&mut self, name: &Tok) -> Result<(), ExprError> {
        if PREDICATES.contains(&name.value.as_str()) {
            return Err(ExprError::at(
                name.at,
                format!("{}() is not supported in rules", name.value),
            ));
        }
        self.count()
    }

    fn call_node(&mut self, name: Tok, args: Vec<Node>) -> Parsed {
        tri!(self.count());
        let callee = Self::node(NodeKind::Ident(name.value), name.at);
        Ok(Self::node(
            NodeKind::Call {
                callee: Box::new(callee),
                args,
            },
            name.at,
        ))
    }

    fn arguments(&mut self) -> Result<Vec<Node>, ExprError> {
        tri!(self.expect(TKind::Bracket, "("));
        let mut args = Vec::new();
        while tri!(self.argument_follows(args.is_empty())) {
            args.push(tri!(self.expression(0)));
        }
        tri!(self.expect(TKind::Bracket, ")"));
        Ok(args)
    }

    /// Whether another argument follows, past its `,` when it is not the
    /// first.
    fn argument_follows(&mut self, first: bool) -> Result<bool, ExprError> {
        if self.current.is(TKind::Bracket, ")") {
            return Ok(false);
        }
        if !first {
            tri!(self.expect(TKind::Operator, ","));
        }
        Ok(!self.current.is(TKind::Bracket, ")"))
    }

    fn postfix(&mut self, mut node: Node) -> Parsed {
        while let Some((prop, optional)) = tri!(self.property()) {
            node = tri!(self.member(node, prop, optional));
            let NodeKind::Member { method, chain, .. } = node.kind else {
                break;
            };
            if method {
                let args = tri!(self.arguments());
                node = tri!(self.method_call(node, args));
            }
            if chain {
                tri!(self.count());
            }
        }
        Ok(node)
    }

    /// The property after a `.` or `?.` at the current token, consumed with
    /// it, and whether the access is optional; `None` when no member access
    /// follows.
    fn property(&mut self) -> Result<Option<(Tok, bool)>, ExprError> {
        if self.current.kind != TKind::Operator && self.current.kind != TKind::Bracket {
            return Ok(None);
        }
        let optional = match self.current.value.as_str() {
            "." => false,
            "?." => true,
            "[" => return Err(self.unsupported()),
            _ => return Ok(None),
        };
        tri!(self.next());
        let prop = self.current.clone();
        if optional && prop.is(TKind::Bracket, "[") {
            return Err(self.unsupported());
        }
        tri!(self.next());
        let valid_name = prop.kind == TKind::Identifier
            || (prop.kind == TKind::Operator
                && prop.value.chars().next().is_some_and(is_alphabetic)
                && prop.value.chars().all(is_alphanumeric));
        if !valid_name {
            return Err(ExprError::at(self.current.at, "expected name"));
        }
        Ok(Some((prop, optional)))
    }

    /// `base.prop`, a method when a `(` follows.
    fn member(&mut self, base: Node, prop: Tok, optional: bool) -> Parsed {
        // expr-lang makes a node for the property name, then the member,
        // the call and a chain node around them.
        tri!(self.count());
        tri!(self.count());
        let chain = optional || is_chain(&base);
        let method = self.current.is(TKind::Bracket, "(");
        Ok(Self::node(
            NodeKind::Member {
                base: Box::new(base),
                prop: prop.value,
                optional,
                method,
                chain,
            },
            prop.at,
        ))
    }

    fn method_call(&mut self, member: Node, args: Vec<Node>) -> Parsed {
        tri!(self.count());
        let at = member.at;
        Ok(Self::node(
            NodeKind::Call {
                callee: Box::new(member),
                args,
            },
            at,
        ))
    }

    /// A chain of comparisons, `1 < x <= 3`, from the operand after the
    /// first operator: one comparison, or several joined by `&&`.
    fn comparison(
        &mut self,
        mut left: Node,
        mut op: &'static str,
        mut at: usize,
        precedence: u16,
    ) -> Parsed {
        let mut root = None;
        loop {
            let comparator = tri!(self.expression(precedence + 1));
            root = Some(tri!(self.comparison_node(root, left, &comparator, op, at)));
            left = comparator;
            let Some((next, next_at)) = tri!(self.next_comparison()) else {
                break;
            };
            (op, at) = (next, next_at);
        }
        Ok(root.unwrap_or(left))
    }

    /// `left op comparator`, joined to the comparisons before it by `&&`.
    fn comparison_node(
        &mut self,
        root: Option<Node>,
        left: Node,
        comparator: &Node,
        op: &'static str,
        at: usize,
    ) -> Parsed {
        tri!(self.count());
        if root.is_some() {
            tri!(self.count());
        }
        let cmp = Self::node(
            NodeKind::Binary {
                op,
                left: Box::new(left),
                right: Box::new(comparator.clone()),
            },
            at,
        );
        Ok(match root {
            None => cmp,
            Some(r) => Self::node(
                NodeKind::Binary {
                    op: "&&",
                    left: Box::new(r),
                    right: Box::new(cmp),
                },
                at,
            ),
        })
    }

    /// The comparison operator at the current token, consumed, if there is
    /// one.
    fn next_comparison(&mut self) -> Result<Option<(&'static str, usize)>, ExprError> {
        if self.current.kind != TKind::Operator || !is_comparison(&self.current.value) {
            return Ok(None);
        }
        let op = binary(&self.current.value).map_or("<", |b| b.0);
        let at = self.current.at;
        tri!(self.next());
        Ok(Some((op, at)))
    }
}

/// A binary operator that `Parser::operation` takes.
struct Operator {
    /// expr-lang's name for it, as written.
    op: &'static str,
    at: usize,
    precedence: u16,
    right_assoc: bool,
    /// Where the `in` (or `contains`, ...) of a `not in` is: the result is
    /// negated there.
    not_at: Option<usize>,
}

fn number(tok: &Tok) -> Parsed {
    let value = tok.value.replace('_', "");
    let lower = value.to_ascii_lowercase();
    let int = |digits: &str, radix: u32, what: &str| -> Parsed {
        i64::from_str_radix(digits, radix)
            .map(|n| Parser::node(NodeKind::Int(n), tok.at))
            .map_err(|e| {
                let why = if matches!(
                    e.kind(),
                    std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow
                ) {
                    "value out of range"
                } else {
                    "invalid syntax"
                };
                ExprError::at(
                    tok.at,
                    format!(
                        "invalid {what} literal: strconv.ParseInt: parsing {}: {why}",
                        go::quote(&value)
                    ),
                )
            })
    };
    if let Some(hex) = lower.strip_prefix("0x") {
        int(hex, 16, "hex")
    } else if lower.contains(['.', 'e']) {
        match value.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Parser::node(NodeKind::Float(f), tok.at)),
            Ok(_) => Err(ExprError::at(
                tok.at,
                format!(
                    "invalid float literal: strconv.ParseFloat: parsing {}: value out of range",
                    go::quote(&value)
                ),
            )),
            Err(_) => Err(ExprError::at(
                tok.at,
                format!(
                    "invalid float literal: strconv.ParseFloat: parsing {}: invalid syntax",
                    go::quote(&value)
                ),
            )),
        }
    } else if let Some(bin) = lower.strip_prefix("0b") {
        int(bin, 2, "binary")
    } else if let Some(oct) = lower.strip_prefix("0o") {
        int(oct, 8, "octal")
    } else {
        int(&value, 10, "integer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> String {
        parse(src).unwrap_err().render(src)
    }

    #[test]
    fn errors_match_expr_lang() {
        assert_eq!(
            err("invalid == ( && "),
            "unexpected token Operator(\"&&\") (1:14)\n | invalid == ( && \n | .............^"
        );
        assert_eq!(err(""), "unexpected token EOF");
        assert_eq!(
            err("a == @b"),
            "unrecognized character: U+0040 '@' (1:6)\n | a == @b\n | .....^"
        );
        assert_eq!(
            err("5abc"),
            "bad number syntax: \"5a\" (1:2)\n | 5abc\n | .^"
        );
        assert_eq!(
            err("a ?= "),
            "unexpected token Operator(\"=\") (1:4)\n | a ?= \n | ...^"
        );
        assert_eq!(
            err("a == "),
            "unexpected token EOF (1:5)\n | a == \n | ....^"
        );
    }

    #[test]
    fn parses_precedence_and_chains() {
        let n = parse("a || b && !like(c, \"x\") == false").unwrap();
        let NodeKind::Binary {
            op: "||", right, ..
        } = &n.kind
        else {
            panic!("{n:?}")
        };
        let NodeKind::Binary {
            op: "&&", right, ..
        } = &right.kind
        else {
            panic!()
        };
        // `!` binds tighter than `==`: its operand stops there.
        let NodeKind::Binary { op: "==", left, .. } = &right.kind else {
            panic!()
        };
        assert!(matches!(left.kind, NodeKind::Unary { op: "!", .. }));
        let n = parse("1 < x <= 3").unwrap();
        assert!(matches!(n.kind, NodeKind::Binary { op: "&&", .. }));
        let n = parse("request.auth.id").unwrap();
        let NodeKind::Member { prop, base, .. } = &n.kind else {
            panic!()
        };
        assert_eq!(prop, "id");
        assert!(matches!(base.kind, NodeKind::Member { .. }));
    }

    #[test]
    fn unescapes_quoted_strings() {
        let n = parse(r#""a\"b\\c\n\x01é""#).unwrap();
        let NodeKind::Str(s) = &n.kind else { panic!() };
        assert_eq!(s, "a\"b\\c\n\u{1}é");
    }

    /// Runs `f` on a 256 KiB stack, a quarter of a browser worker's.
    fn on_small_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(f)
            .expect("spawn a thread")
            .join()
            .expect("the thread finishes");
    }

    #[test]
    fn counts_nodes_like_expr_lang() {
        // The longest expressions expr-lang v1.17.8 accepts, and where it
        // fails on one node more, recorded from it.
        let cases = [
            // A member is a node for its name and one for the access.
            ("a".to_owned(), ".b", 4_999, 10_001),
            // After `?.` every access is wrapped in a chain node too, also
            // through parentheses.
            ("a?.b".to_owned(), ".c", 3_332, 6_670),
            ("(a?.b)".to_owned(), ".c", 3_332, 6_672),
            // `not in` is an `in` under a `not`.
            ("x not in y".to_owned(), " && x not in y", 1_999, 28_010),
            ("a".to_owned(), " ?? a", 4_999, 25_001),
        ];
        on_small_stack(move || {
            for (head, tail, most, column) in cases {
                let src = format!("{head}{}", tail.repeat(most));
                assert!(parse(&src).is_ok(), "{head}{tail} x {most}");
                let src = format!("{src}{tail}");
                assert_eq!(
                    err(&src),
                    format!(
                        "{TOO_MANY_NODES} (1:{column})\n | {src}\n | {}^",
                        ".".repeat(column - 1)
                    )
                );
            }
        });
    }

    #[test]
    fn refuses_deep_nesting() {
        on_small_stack(|| {
            let nested = |depth: usize| format!("{}a{}", "(".repeat(depth), ")".repeat(depth));
            // The outer expression and one per parenthesis.
            assert!(parse(&nested(crate::MAX_DEPTH - 1)).is_ok());
            let src = nested(crate::MAX_DEPTH);
            let at = crate::MAX_DEPTH + 1;
            assert_eq!(
                err(&src),
                format!("{TOO_DEEP} (1:{at})\n | {src}\n | {}^", ".".repeat(at - 1))
            );
            for src in [
                nested(100_000),
                "!".repeat(100_000) + "a",
                "f(".repeat(100_000),
            ] {
                assert!(err(&src).starts_with(TOO_DEEP), "{}", &src[..10]);
            }
        });
    }

    #[test]
    fn long_chains_copy_and_drop_on_a_small_stack() {
        on_small_stack(|| {
            // `?:` copies its condition, a comparison chain its middle
            // operand: here chains thousands of nodes deep.
            let chain = vec!["a"; 4_000].join(" && ");
            let n = parse(&format!("{chain} ?: b")).unwrap();
            let copy = n.clone();
            drop(n);
            assert!(matches!(copy.kind, NodeKind::Cond { .. }));
            let path = ".a".repeat(4_000);
            let n = parse(&format!("x < y{path} < z")).unwrap();
            let NodeKind::Binary {
                op: "&&", right, ..
            } = &n.kind
            else {
                panic!()
            };
            let NodeKind::Binary { left, .. } = &right.kind else {
                panic!()
            };
            assert!(matches!(left.kind, NodeKind::Member { .. }));
        });
    }
}
