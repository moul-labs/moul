//! Dates and times as the Go engine writes and accepts them, without a
//! calendar library.

/// Days from 1970-01-01 to the civil date `y-m-d` (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// `ms` since the epoch as RFC 3339 in UTC with whole seconds, the way the
/// Go engine stamps `createdAt`: `2026-11-18T00:32:44Z`.
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parses `YYYY-MM-DD` into days since the epoch, checking the date exists.
pub fn parse_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = digits(&s[0..4])?;
    let m = digits(&s[5..7])? as u32;
    let d = digits(&s[8..10])? as u32;
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Parses a date-time the Go engine's `datetime` fields accept: RFC 3339
/// with or without fractions and with `Z` or an offset, or
/// `YYYY-MM-DDTHH:MM:SS` / `YYYY-MM-DD HH:MM:SS` without a zone (read as
/// UTC). Returns milliseconds since the epoch.
pub fn parse_datetime(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let days = parse_date(s.get(0..10)?)?;
    let sep = s.as_bytes()[10];
    if sep != b'T' && sep != b' ' {
        return None;
    }
    let t = s.get(11..19)?;
    let tb = t.as_bytes();
    if tb[2] != b':' || tb[5] != b':' {
        return None;
    }
    let (h, mi, se) = (digits(&t[0..2])?, digits(&t[3..5])?, digits(&t[6..8])?);
    if h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let mut rest = &s[19..];
    let mut ms = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 || n > 9 {
            return None;
        }
        let f = &frac[..n];
        ms = digits(&format!("{:0<3}", &f[..n.min(3)]))?;
        rest = &frac[n..];
    }
    let offset_min = match rest {
        "" => 0,
        "Z" => 0,
        _ => {
            let rb = rest.as_bytes();
            if rb.len() != 6 || (rb[0] != b'+' && rb[0] != b'-') || rb[3] != b':' {
                return None;
            }
            let oh = digits(&rest[1..3])?;
            let om = digits(&rest[4..6])?;
            // Go's layouts accept offsets up to 24:60.
            if oh > 24 || om > 60 {
                return None;
            }
            let v = oh * 60 + om;
            if rb[0] == b'-' { -v } else { v }
        }
    };
    // A zone-less "T" form is accepted only without fractions, as in Go's
    // list of layouts; a space form never takes a zone.
    if sep == b' ' && !rest.is_empty() {
        return None;
    }
    let secs = days * 86_400 + h * 3600 + mi * 60 + se - offset_min * 60;
    Some(secs * 1000 + ms)
}

/// Parses strict RFC 3339 (a zone is required), as Go's `time.RFC3339`
/// layout does. Returns milliseconds since the epoch.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let last = s.as_bytes().last()?;
    if !(*last == b'Z' || s.len() >= 25 && s.as_bytes()[s.len() - 3] == b':') {
        return None;
    }
    if s.as_bytes().get(10) == Some(&b' ') {
        return None;
    }
    parse_datetime(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses() {
        let ms = 1_794_961_964_000; // 2026-11-18T00:32:44Z
        assert_eq!(rfc3339(ms), "2026-11-18T00:32:44Z");
        assert_eq!(parse_rfc3339("2026-11-18T00:32:44Z"), Some(ms));
        assert_eq!(parse_datetime("2026-11-18 00:32:44"), Some(ms));
        assert_eq!(parse_datetime("2026-11-18T02:32:44+02:00"), Some(ms));
        assert_eq!(parse_datetime("2026-11-18T00:32:44.5Z"), Some(ms + 500));
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn rejects_bad_dates() {
        assert!(parse_date("2026-02-30").is_none());
        assert!(parse_date("2024-02-29").is_some());
        assert!(parse_date("26-02-01").is_none());
        assert!(parse_datetime("2026-11-18T25:00:00Z").is_none());
        assert!(parse_datetime("yesterday").is_none());
        assert!(parse_rfc3339("2026-11-18T00:32:44").is_none());
        // Go's layouts take an uppercase T and Z only, and offsets to 24h.
        assert!(parse_datetime("2026-11-18t00:32:44Z").is_none());
        assert!(parse_datetime("2026-11-18T00:32:44z").is_none());
        assert!(parse_datetime("2026-11-18T00:32:44+24:00").is_some());
    }
}
