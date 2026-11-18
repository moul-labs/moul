//! Just enough XML for S3's ListObjectsV2 answer and its error documents.

/// The text of each `<tag>` element directly or indirectly inside `xml`, in
/// order. S3's documents have no attributes on these tags and no CDATA.
pub(crate) fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

/// The text of the first `<tag>` element, unescaped.
pub(crate) fn first(xml: &str, tag: &str) -> Option<String> {
    elements(xml, tag).first().map(|s| unescape(s))
}

/// Replaces XML's five entities and numeric character references.
pub(crate) fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(end) = after.find(';') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let entity = &after[..end];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&rest[i..i + 2 + end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_answer() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name>
<Contents><Key>p/ltx/0/a&amp;b.ltx</Key><LastModified>2026-11-18T03:04:05.000Z</LastModified><Size>650</Size></Contents>
<Contents><Key>p/moul/head.json</Key><LastModified>2026-11-18T03:04:06.000Z</LastModified><Size>80</Size></Contents>
<IsTruncated>true</IsTruncated><NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken></ListBucketResult>"#;
        let contents = elements(xml, "Contents");
        assert_eq!(contents.len(), 2);
        assert_eq!(first(contents[0], "Key").unwrap(), "p/ltx/0/a&b.ltx");
        assert_eq!(first(contents[1], "Size").unwrap(), "80");
        assert_eq!(first(xml, "IsTruncated").unwrap(), "true");
        assert_eq!(unescape("&#x41;&#66;&bogus;"), "AB&bogus;");
    }
}
