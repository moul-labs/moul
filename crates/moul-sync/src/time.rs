//! UTC dates without a date library: what SigV4 and S3 listings need.

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

/// Splits milliseconds since the epoch into date and time of day.
fn parts(ms: i64) -> (i64, u32, u32, i64, i64, i64, i64) {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    (y, m, d, t / 3600, t / 60 % 60, t % 60, ms.rem_euclid(1000))
}

/// `20130524T000000Z`, the form SigV4 signs.
pub(crate) fn amz_date(ms: i64) -> String {
    let (y, m, d, h, mi, s, _) = parts(ms);
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// `2026-11-18T03:04:05.123Z`.
pub fn rfc3339(ms: i64) -> String {
    let (y, m, d, h, mi, s, milli) = parts(ms);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// Parses `2026-11-18T03:04:05Z`, with optional fractional seconds and an
/// optional `+hh:mm` offset, into milliseconds since the epoch.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut millis = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        if frac.is_empty() {
            return None;
        }
        let padded = format!("{frac:0<3}");
        millis = padded[..3].parse().ok()?;
    }
    let offset = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(&sign @ (b'+' | b'-')) if b.len() == i + 6 && b[i + 3] == b':' => {
            let oh = num(i + 1..i + 3)?;
            let om = num(i + 4..i + 6)?;
            let o = (oh * 60 + om) * 60_000;
            if sign == b'+' { o } else { -o }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000 + millis) - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(amz_date(1_369_353_600_000), "20130524T000000Z");
        for ms in [0, 951_782_400_000, 1_731_500_000_123, 4_102_444_800_000] {
            assert_eq!(parse_rfc3339(&rfc3339(ms)), Some(ms), "{}", rfc3339(ms));
        }
        assert_eq!(
            parse_rfc3339("2026-11-18T03:04:05Z"),
            parse_rfc3339("2026-11-18T10:04:05+07:00")
        );
        assert_eq!(
            parse_rfc3339("2026-11-18T03:04:05.5Z").map(|m| m % 1000),
            Some(500)
        );
        assert_eq!(parse_rfc3339("2026-13-18T03:04:05Z"), None);
        assert_eq!(parse_rfc3339("yesterday"), None);
    }
}
