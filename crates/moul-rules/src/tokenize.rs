//! The rule tokenizer, Go's `rules.Tokenize`. Both the evaluator and the
//! SQL compiler start from these tokens.

use crate::go;

/// What a token is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Identifier,
    Operator,
    String,
    Number,
    Paren,
    Logical,
    Comma,
}

/// One token. A string token holds its unescaped value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: Kind,
    pub value: String,
}

impl Token {
    fn new(kind: Kind, value: impl Into<String>) -> Token {
        Token {
            kind,
            value: value.into(),
        }
    }

    pub fn is(&self, kind: Kind) -> bool {
        self.kind == kind
    }
}

fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn is_identifier_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '@'
}

fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-' | '@')
}

/// Splits a rule into tokens. Positions in errors count characters.
pub(crate) fn tokenize(rule: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = rule.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    let text = |from: usize, to: usize| chars[from..to].iter().collect::<String>();

    while i < n {
        let c = chars[i];
        if is_whitespace(c) {
            i += 1;
            continue;
        }
        // A comment runs to the end of the line.
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            i += 2;
            while i < n && chars[i] != '\n' && chars[i] != '\r' {
                i += 1;
            }
            continue;
        }
        if c == '(' || c == ')' {
            tokens.push(Token::new(Kind::Paren, c));
            i += 1;
            continue;
        }
        if c == ',' {
            tokens.push(Token::new(Kind::Comma, c));
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            let start = i;
            i += 1;
            let mut value = String::new();
            while i < n && chars[i] != c {
                if chars[i] == '\\' && i + 1 < n {
                    value.push(chars[i + 1]);
                    i += 2;
                } else {
                    value.push(chars[i]);
                    i += 1;
                }
            }
            if i >= n {
                return Err(format!(
                    "unterminated string literal starting at position {start}"
                ));
            }
            i += 1;
            tokens.push(Token::new(Kind::String, value));
            continue;
        }
        // Operators, longest first.
        if i + 3 <= n {
            let three = text(i, i + 3);
            if matches!(three.as_str(), "?!=" | "?!~" | "?>=" | "?<=") {
                tokens.push(Token::new(Kind::Operator, three));
                i += 3;
                continue;
            }
        }
        if i + 2 <= n {
            let two = text(i, i + 2);
            match two.as_str() {
                "&&" | "||" => {
                    tokens.push(Token::new(Kind::Logical, two));
                    i += 2;
                    continue;
                }
                "?=" | "?~" | "?>" | "?<" | "!=" | "==" | ">=" | "<=" | "!~" => {
                    tokens.push(Token::new(Kind::Operator, two));
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        if matches!(c, '=' | '~' | '>' | '<') {
            tokens.push(Token::new(Kind::Operator, c));
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '-' && i + 1 < n && chars[i + 1].is_ascii_digit()) {
            let start = i;
            if c == '-' {
                i += 1;
            }
            while i < n && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            tokens.push(Token::new(Kind::Number, text(start, i)));
            continue;
        }
        if is_identifier_start(c) {
            let start = i;
            while i < n && is_identifier_char(chars[i]) {
                i += 1;
            }
            let value = text(start, i);
            let kind = if value.eq_ignore_ascii_case("and") || value.eq_ignore_ascii_case("or") {
                Kind::Logical
            } else {
                Kind::Identifier
            };
            tokens.push(Token::new(kind, value));
            continue;
        }
        return Err(format!(
            "unexpected character {} at position {i}",
            go::quote_rune(c)
        ));
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(rule: &str) -> Vec<(Kind, String)> {
        tokenize(rule)
            .unwrap()
            .into_iter()
            .map(|t| (t.kind, t.value))
            .collect()
    }

    #[test]
    fn splits_a_rule() {
        use Kind::*;
        assert_eq!(
            kinds("@request.auth.id != '' && (tags ?= 'a' OR n>=-1.5) // owner"),
            vec![
                (Identifier, "@request.auth.id".into()),
                (Operator, "!=".into()),
                (String, "".into()),
                (Logical, "&&".into()),
                (Paren, "(".into()),
                (Identifier, "tags".into()),
                (Operator, "?=".into()),
                (String, "a".into()),
                (Logical, "OR".into()),
                (Identifier, "n".into()),
                (Operator, ">=".into()),
                (Number, "-1.5".into()),
                (Paren, ")".into()),
            ]
        );
        assert_eq!(kinds(r#""it\"s""#), vec![(String, "it\"s".into())]);
    }

    #[test]
    fn reports_errors_like_go() {
        assert_eq!(
            tokenize("a = 'x").unwrap_err(),
            "unterminated string literal starting at position 4"
        );
        assert_eq!(
            tokenize("a ! b").unwrap_err(),
            "unexpected character '!' at position 2"
        );
    }
}
