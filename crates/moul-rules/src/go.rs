//! Small pieces of Go's standard library the rule engine's messages and
//! values depend on: `%q`, `%v` of a float, and the date macros.

use std::fmt::Write;

/// `strconv.Quote`: a double-quoted string with Go escapes.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        push_escaped(&mut out, c, '"');
    }
    out.push('"');
    out
}

/// `strconv.QuoteRune`: a single-quoted rune with Go escapes.
pub(crate) fn quote_rune(c: char) -> String {
    let mut out = String::from("'");
    push_escaped(&mut out, c, '\'');
    out.push('\'');
    out
}

fn push_escaped(out: &mut String, c: char, quote: char) {
    match c {
        _ if c == quote => {
            out.push('\\');
            out.push(c);
        }
        '\\' => out.push_str("\\\\"),
        '\u{7}' => out.push_str("\\a"),
        '\u{8}' => out.push_str("\\b"),
        '\u{c}' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{b}' => out.push_str("\\v"),
        _ if (c as u32) < 0x20 || c as u32 == 0x7f => {
            let _ = write!(out, "\\x{:02x}", c as u32);
        }
        _ if !is_print(c) => {
            if (c as u32) < 0x10000 {
                let _ = write!(out, "\\u{:04x}", c as u32);
            } else {
                let _ = write!(out, "\\U{:08x}", c as u32);
            }
        }
        _ => out.push(c),
    }
}

/// An approximation of `unicode.IsPrint` without Unicode tables: control
/// and invisible formatting characters are not printable.
fn is_print(c: char) -> bool {
    !(c.is_control()
        || matches!(c as u32,
            0xad | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x180e
            | 0x2000..=0x200f | 0x2028..=0x202f | 0x205f..=0x2064 | 0x2066..=0x206f
            | 0x3000 | 0xfeff | 0xfff9..=0xfffb | 0xe000..=0xf8ff | 0xf0000..)
        || c == '\u{a0}')
}

/// `%#U` of a rune: `U+0040 '@'`.
pub(crate) fn rune_u(c: char) -> String {
    if is_print(c) {
        format!("U+{:04X} '{c}'", c as u32)
    } else {
        format!("U+{:04X}", c as u32)
    }
}

/// `%v` of a `float64`: the shortest digits that round-trip, in exponent
/// form when the exponent is below -4 or at least 6, as `%g` does.
pub(crate) fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0" } else { "0" }.into();
    }
    // Rust's `{:e}` gives the shortest digits that round-trip, as Go's does.
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let neg = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let nd = digits.len() as i32;
    let dp = exp + 1;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if !(-4..6).contains(&exp) {
        out.push_str(&digits[..1]);
        if nd > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    } else if dp <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-dp) as usize));
        out.push_str(&digits);
    } else if dp >= nd {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (dp - nd) as usize));
    } else {
        out.push_str(&digits[..dp as usize]);
        out.push('.');
        out.push_str(&digits[dp as usize..]);
    }
    out
}

/// `%v` of a JSON value as Go decodes it: numbers are `float64`, objects
/// are maps printed with sorted keys.
pub(crate) fn format_json(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "<nil>".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => format_float(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(format_json).collect();
            format!("[{}]", parts.join(" "))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{k}:{}", format_json(&map[*k])))
                .collect();
            format!("map[{}]", parts.join(" "))
        }
    }
}

/// Go's `ParseFloat`, for strings that rules compare as numbers: `None`
/// where Go returns an error. Hex floats are not read; nothing else that Go
/// accepts is refused.
pub(crate) fn parse_float(s: &str) -> Option<f64> {
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    // Go reads `NaN` only without a sign.
    if unsigned.len() < s.len() && unsigned.eq_ignore_ascii_case("nan") {
        return None;
    }
    // Go allows `_` between two digits, as in `1_000`.
    let f = if s.contains('_') {
        let b = s.as_bytes();
        let between_digits = |i: usize| {
            i > 0 && b[i - 1].is_ascii_digit() && b.get(i + 1).is_some_and(u8::is_ascii_digit)
        };
        if !(0..b.len()).all(|i| b[i] != b'_' || between_digits(i)) {
            return None;
        }
        s.replace('_', "").parse::<f64>().ok()?
    } else {
        s.parse::<f64>().ok()?
    };
    // Past the largest float64 Go fails with ErrRange; only `Inf` and
    // `Infinity` themselves are infinite.
    if f.is_infinite()
        && !unsigned.eq_ignore_ascii_case("inf")
        && !unsigned.eq_ignore_ascii_case("infinity")
    {
        return None;
    }
    Some(f)
}

/// The table name rule of the Go engine: a letter, then up to 62 letters,
/// digits or underscores.
pub(crate) fn validate_table_name(name: &str) -> Result<(), String> {
    if moul_store::schema::is_valid_collection_name(name) {
        Ok(())
    } else {
        Err(format!(
            "invalid table name {}: must start with a letter and contain only letters, digits, or underscores (max 63 chars)",
            quote(name)
        ))
    }
}

/// Whether `s` is a plain column name: `^[A-Za-z_][A-Za-z0-9_]*$`.
pub(crate) fn is_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// The date macros (`@now`, `@todayStart`, ...) for a moment in time, as
/// the Go engine formats them: `2006-01-02 15:04:05.000Z`, UTC.
pub(crate) fn date_macro(name: &str, now_ms: i64) -> Option<String> {
    const DAY: i64 = 86_400_000;
    const END_OF_DAY: i64 = DAY - 1;
    let day = now_ms.div_euclid(DAY);
    let (y, m, _) = civil_from_days(day);
    let ms = match name {
        "now" => now_ms,
        "yesterday" => now_ms - DAY,
        "tomorrow" => now_ms + DAY,
        "todayStart" => day * DAY,
        "todayEnd" => day * DAY + END_OF_DAY,
        "monthStart" => days_from_civil(y, m, 1) * DAY,
        "monthEnd" => {
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            (days_from_civil(ny, nm, 1) - 1) * DAY + END_OF_DAY
        }
        "yearStart" => days_from_civil(y, 1, 1) * DAY,
        "yearEnd" => days_from_civil(y, 12, 31) * DAY + END_OF_DAY,
        _ => return None,
    };
    Some(format_time(ms))
}

fn format_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}Z",
        t / 3600,
        t / 60 % 60,
        t % 60,
        ms.rem_euclid(1000)
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date for days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_like_go() {
        assert_eq!(format_float(50.0), "50");
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(123456.0), "123456");
        assert_eq!(format_float(1_000_000.0), "1e+06");
        assert_eq!(format_float(1_234_567.0), "1.234567e+06");
        assert_eq!(format_float(0.0001), "0.0001");
        assert_eq!(format_float(0.00001), "1e-05");
        assert_eq!(format_float(-2.5), "-2.5");
    }

    #[test]
    fn floats_parse_like_go() {
        // Go's ParseFloat answers these with ±Inf and ErrRange.
        for s in [
            "1e400",
            "-1e999",
            "1.8e308",
            "1.7976931348623159e308",
            "1_000e400",
        ] {
            assert_eq!(parse_float(s), None, "{s}");
        }
        assert_eq!(parse_float("1.7976931348623157e308"), Some(f64::MAX));
        assert_eq!(parse_float("inf"), Some(f64::INFINITY));
        assert_eq!(parse_float("+INF"), Some(f64::INFINITY));
        assert_eq!(parse_float("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_float("infinit"), None);
        // Underflow is not an error.
        assert_eq!(parse_float("1e-400"), Some(0.0));
        assert!(parse_float("NaN").is_some_and(f64::is_nan));
        assert_eq!(parse_float("-nan"), None);
        assert_eq!(parse_float("+NaN"), None);
        // `_` only between digits.
        assert_eq!(parse_float("1_000"), Some(1000.0));
        assert_eq!(parse_float("1_0.0_1e1_0"), Some(1.001e11));
        assert_eq!(parse_float("+1_0"), Some(10.0));
        for s in [
            "_1", "1_", "1__0", "1_.5", "1._5", "1_e5", "1e_5", "-_1", "_",
        ] {
            assert_eq!(parse_float(s), None, "{s}");
        }
        assert_eq!(parse_float(".5"), Some(0.5));
        assert_eq!(parse_float("1e"), None);
        assert_eq!(parse_float(" 1"), None);
    }

    #[test]
    fn quotes_like_go() {
        assert_eq!(quote("a\"b\\c\n"), r#""a\"b\\c\n""#);
        assert_eq!(quote("\u{1}"), r#""\x01""#);
        assert_eq!(quote_rune('\''), r"'\''");
        assert_eq!(quote_rune('!'), "'!'");
        assert_eq!(rune_u('@'), "U+0040 '@'");
    }

    #[test]
    fn date_macros() {
        // 2026-11-18 12:34:56.789 UTC
        let now = 1_795_005_296_789;
        assert_eq!(date_macro("now", now).unwrap(), "2026-11-18 12:34:56.789Z");
        assert_eq!(
            date_macro("yesterday", now).unwrap(),
            "2026-11-17 12:34:56.789Z"
        );
        assert_eq!(
            date_macro("todayStart", now).unwrap(),
            "2026-11-18 00:00:00.000Z"
        );
        assert_eq!(
            date_macro("todayEnd", now).unwrap(),
            "2026-11-18 23:59:59.999Z"
        );
        assert_eq!(
            date_macro("monthStart", now).unwrap(),
            "2026-11-01 00:00:00.000Z"
        );
        assert_eq!(
            date_macro("monthEnd", now).unwrap(),
            "2026-11-30 23:59:59.999Z"
        );
        assert_eq!(
            date_macro("yearEnd", now).unwrap(),
            "2026-12-31 23:59:59.999Z"
        );
        assert_eq!(date_macro("later", now), None);
    }
}
