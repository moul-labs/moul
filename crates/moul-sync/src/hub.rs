//! A replica behind a Moul Hub: the hub's replica calls over HTTP, with the
//! device's token.
//!
//! | Replica call | Request |
//! | --- | --- |
//! | `get` | `GET /api/hub/db/<name>/o/<key>`: the bytes and `ETag`, or `404` |
//! | `put` | `PUT /api/hub/db/<name>/o/<key>` with `If-None-Match: *` or `If-Match: "<etag>"`; `412` when the condition fails |
//! | `list` | `GET /api/hub/db/<name>/list?prefix=<p>`: `{"items": [{"key", "size", "modified"}]}` |
//! | `delete` | `DELETE /api/hub/db/<name>/o/<key>`: `204`, also for a missing key |
//!
//! Every request carries `Authorization: Bearer <token>`. The hub checks each
//! condition under one lock per database, so two devices can never both
//! write a TXID, whatever store is behind it.
//!
//! The hub compacts and prunes the replicas it fronts, so a [`Sync`](crate::Sync)
//! through a hub leaves that to it ([`Replica::maintained_remotely`]).
//! Followers hear of new changes from the event stream at
//! [`HubReplica::events_url`], which [`EventParser`] reads.
//!
//! In a browser the hub must answer the page's origin with CORS: allow the
//! `Authorization`, `If-Match`, `If-None-Match` and `Content-Type` request
//! headers, and expose `ETag`, which conditional writes depend on.

use serde::Deserialize;

use crate::replica::{BoxFuture, Condition, Object, ObjectInfo, Put, Replica, check_key};
use crate::s3::{HttpClient, HttpRequest, HttpResponse};
use crate::time::parse_rfc3339;
use crate::{Error, ErrorKind, Result};

/// Where a hub is, which database, and the device's token.
#[derive(Clone, PartialEq, Eq)]
pub struct HubConfig {
    /// The hub's address, such as `https://sync.moul.dev`. Paths start with
    /// `/api/hub` under it.
    pub url: String,
    /// The database on the hub: 1 to 63 of `a-z`, `0-9` and `-`, starting
    /// with a letter or digit.
    pub database: String,
    /// The device's token (`mh_…`): an account token, or a database token
    /// from a join link.
    pub token: String,
}

impl std::fmt::Debug for HubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubConfig")
            .field("url", &self.url)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

impl HubConfig {
    /// Reads `moul+https://host/<database>` (or `moul+http://…`, and a hub
    /// under a path, `moul+https://host/base/<database>`): the form a
    /// [`HubReplica`] describes itself with.
    pub fn from_url(url: &str, token: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("moul+")
            .ok_or_else(|| Error::invalid(format!("{url:?} is not a moul+https:// URL")))?;
        let rest = rest.trim_end_matches('/');
        let (hub, database) = rest
            .rsplit_once('/')
            .filter(|(hub, _)| !hub.ends_with('/'))
            .ok_or_else(|| Error::invalid(format!("{url:?} names no database")))?;
        let config = Self {
            url: hub.to_owned(),
            database: database.to_owned(),
            token: token.to_owned(),
        };
        config.check()?;
        Ok(config)
    }

    /// The hub's address without a trailing `/`, checked.
    fn check(&self) -> Result<&str> {
        let base = self.url.trim_end_matches('/');
        let rest = base
            .strip_prefix("https://")
            .or_else(|| base.strip_prefix("http://"))
            .ok_or_else(|| {
                Error::invalid(format!("the hub address {:?} is not http(s)", self.url))
            })?;
        let host = rest.split('/').next().unwrap_or_default();
        if host.is_empty() || host.contains('@') || base.contains(['?', '#']) {
            return Err(Error::invalid(format!(
                "the hub address {:?} is not a plain http(s) address",
                self.url
            )));
        }
        if !valid_database_name(&self.database) {
            return Err(Error::invalid(format!(
                "{:?} is not a hub database name: 1 to 63 of a-z, 0-9 and -, starting with a \
                 letter or digit",
                self.database
            )));
        }
        if self.token.is_empty() || self.token.chars().any(|c| c.is_control() || c == ' ') {
            return Err(Error::invalid("a hub replica needs the device's token"));
        }
        Ok(base)
    }
}

/// Whether `name` is a database name a hub takes: 1 to 63 of `a-z`, `0-9`
/// and `-`, starting with a letter or digit.
pub fn valid_database_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=63).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Percent-encodes everything but unreserved characters, and `/` when
/// `keep_slash`.
fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// An entity tag without its quotes (or the `W/` of a weak one).
fn unquote(etag: &str) -> String {
    let t = etag.trim();
    let t = t.strip_prefix("W/").unwrap_or(t);
    t.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(t)
        .to_owned()
}

/// A time as the hub sends it: milliseconds since the epoch, or RFC 3339.
fn millis(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => parse_rfc3339(s).or_else(|| s.parse().ok()),
        _ => None,
    }
}

/// A one-time ticket another device trades for a token of its own, through
/// `POST /api/hub/join`.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinTicket {
    /// The ticket.
    pub ticket: String,
    /// When it expires, in milliseconds since the epoch; 0 if the hub did
    /// not say.
    pub expires_at: i64,
}

impl std::fmt::Debug for JoinTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinTicket")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A replica a Moul Hub fronts.
pub struct HubReplica {
    config: HubConfig,
    client: Box<dyn HttpClient>,
    /// The hub's address without a trailing `/`.
    base: String,
}

impl std::fmt::Debug for HubReplica {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubReplica")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Listing {
    items: Vec<ListItem>,
}

#[derive(Deserialize)]
struct ListItem {
    key: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    modified: serde_json::Value,
}

impl HubReplica {
    /// The database `config` names, reached through `client`.
    pub fn new(config: HubConfig, client: impl HttpClient + 'static) -> Result<Self> {
        let base = config.check()?.to_owned();
        Ok(Self {
            config,
            client: Box::new(client),
            base,
        })
    }

    /// A replica on the platform's HTTP client: `ureq` natively, `fetch` in
    /// the browser.
    pub fn with_default_client(config: HubConfig) -> Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        let client = crate::s3::UreqClient::default();
        #[cfg(target_arch = "wasm32")]
        let client = crate::s3::FetchClient;
        Self::new(config, client)
    }

    /// The settings, token included.
    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    fn database_path(&self) -> String {
        format!("{}/api/hub/db/{}", self.base, self.config.database)
    }

    fn object_url(&self, key: &str) -> String {
        format!("{}/o/{}", self.database_path(), encode(key, true))
    }

    /// `GET /api/hub/db/<name>/events`: server-sent events, `head` each time
    /// a device writes `moul/head.json`. Send the token as
    /// `Authorization: Bearer <token>` and read it with [`EventParser`].
    pub fn events_url(&self) -> String {
        format!("{}/events", self.database_path())
    }

    fn request(
        &self,
        method: &'static str,
        url: String,
        extra: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> HttpRequest {
        let mut headers = vec![(
            "authorization".to_owned(),
            format!("Bearer {}", self.config.token),
        )];
        headers.extend(extra);
        HttpRequest {
            method,
            url,
            headers,
            body,
        }
    }

    /// The error for an answer that is not one the call expects. Never
    /// includes the token.
    fn failure(&self, what: &str, r: &HttpResponse) -> Error {
        let message = serde_json::from_slice::<serde_json::Value>(&r.body)
            .ok()
            .and_then(|v| v.get("message")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                let text = String::from_utf8_lossy(&r.body);
                text.chars().take(200).collect::<String>().trim().to_owned()
            });
        let kind = match r.status {
            401 | 403 => ErrorKind::Denied,
            404 => ErrorKind::NotFound,
            408 | 429 | 500..=599 => ErrorKind::Network,
            409 => ErrorKind::Conflict,
            400 | 413 => ErrorKind::Invalid,
            _ => ErrorKind::Replica,
        };
        let hint = match r.status {
            401 => " (the hub no longer takes this device's token: sign in again)",
            _ => "",
        };
        let detail = format!("{what} in {}: {} {message}", self.describe(), r.status);
        Error::new(kind, format!("{}{hint}", detail.trim_end()))
    }

    async fn get_now(&self, key: &str) -> Result<Option<Object>> {
        check_key(key)?;
        let r = self
            .client
            .send(self.request("GET", self.object_url(key), vec![], vec![]))
            .await?;
        match r.status {
            200 => Ok(Some(Object {
                etag: unquote(r.header("etag").unwrap_or_default()),
                bytes: r.body,
            })),
            404 => Ok(None),
            _ => Err(self.failure(&format!("GET {key}"), &r)),
        }
    }

    async fn put_now(&self, key: &str, bytes: &[u8], condition: Condition) -> Result<Put> {
        check_key(key)?;
        let mut extra = vec![(
            "content-type".to_owned(),
            "application/octet-stream".to_owned(),
        )];
        match &condition {
            Condition::None => {}
            Condition::IfNoneMatch => extra.push(("if-none-match".to_owned(), "*".to_owned())),
            Condition::IfMatch(etag) => {
                extra.push(("if-match".to_owned(), format!("\"{}\"", unquote(etag))))
            }
        }
        let r = self
            .client
            .send(self.request("PUT", self.object_url(key), extra, bytes.to_vec()))
            .await?;
        match r.status {
            200..=299 => {
                let etag = match r.header("etag") {
                    Some(e) if !e.is_empty() => unquote(e),
                    // A page reads the header only if the hub exposes it;
                    // the body says it too.
                    _ => serde_json::from_slice::<serde_json::Value>(&r.body)
                        .ok()
                        .and_then(|v| v.get("etag")?.as_str().map(unquote))
                        .unwrap_or_default(),
                };
                Ok(Put::Written { etag })
            }
            412 => Ok(Put::Refused),
            _ => Err(self.failure(&format!("PUT {key}"), &r)),
        }
    }

    async fn list_now(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let url = format!(
            "{}/list?prefix={}",
            self.database_path(),
            encode(prefix, false)
        );
        let r = self
            .client
            .send(self.request(
                "GET",
                url,
                vec![("accept".to_owned(), "application/json".to_owned())],
                vec![],
            ))
            .await?;
        if r.status != 200 {
            return Err(self.failure(&format!("LIST {prefix}"), &r));
        }
        let listing: Listing = serde_json::from_slice(&r.body).map_err(|e| {
            Error::replica(format!(
                "the listing of {prefix:?} in {} is not valid: {e}",
                self.describe()
            ))
        })?;
        let mut out: Vec<ObjectInfo> = listing
            .items
            .into_iter()
            .map(|i| ObjectInfo {
                modified: millis(&i.modified).unwrap_or(0),
                key: i.key,
                size: i.size,
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn delete_now(&self, key: &str) -> Result<()> {
        check_key(key)?;
        let r = self
            .client
            .send(self.request("DELETE", self.object_url(key), vec![], vec![]))
            .await?;
        match r.status {
            200..=299 => Ok(()),
            _ => Err(self.failure(&format!("DELETE {key}"), &r)),
        }
    }

    /// `POST /api/hub/databases/<name>/joins`: a ticket that lets one more
    /// device join this database, valid for 15 minutes. Put it in a join
    /// link; the device that opens it trades it for a token of its own, so
    /// the link never carries this device's token.
    pub async fn create_join_ticket(&self) -> Result<JoinTicket> {
        let url = format!(
            "{}/api/hub/databases/{}/joins",
            self.base, self.config.database
        );
        let r = self
            .client
            .send(self.request(
                "POST",
                url,
                vec![("accept".to_owned(), "application/json".to_owned())],
                vec![],
            ))
            .await?;
        if !(200..=299).contains(&r.status) {
            return Err(self.failure("a join ticket", &r));
        }
        let v: serde_json::Value = serde_json::from_slice(&r.body).map_err(|e| {
            Error::replica(format!(
                "the join ticket from {} is not valid: {e}",
                self.describe()
            ))
        })?;
        let ticket = v
            .get("ticket")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                Error::replica(format!("{} answered without a ticket", self.describe()))
            })?;
        Ok(JoinTicket {
            ticket: ticket.to_owned(),
            expires_at: v.get("expiresAt").and_then(millis).unwrap_or(0),
        })
    }
}

impl Replica for HubReplica {
    /// `moul+https://host/<database>`: never the token.
    fn describe(&self) -> String {
        format!("moul+{}/{}", self.base, self.config.database)
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Object>>> {
        Box::pin(self.get_now(key))
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        condition: Condition,
    ) -> BoxFuture<'a, Result<Put>> {
        Box::pin(self.put_now(key, bytes, condition))
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<ObjectInfo>>> {
        Box::pin(self.list_now(prefix))
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.delete_now(key))
    }

    /// The hub compacts and prunes on its own clock.
    fn maintained_remotely(&self) -> bool {
        true
    }
}

/// One server-sent event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubEvent {
    /// Its type: `connected`, `head`, or `message` when the stream names
    /// none.
    pub event: String,
    /// Its data lines, joined with `\n`.
    pub data: String,
}

/// The longest line kept; a longer one is dropped. A head record is a few
/// hundred bytes.
const MAX_LINE: usize = 1 << 20;

/// Reads a stream of server-sent events, such as the hub's
/// `GET /api/hub/db/<name>/events`, from chunks as they arrive. Lines end
/// with `\n` or `\r\n`; comments (`: ping`) and the `id` and `retry`
/// fields are skipped.
#[derive(Debug, Default)]
pub struct EventParser {
    line: Vec<u8>,
    /// The current line is past [`MAX_LINE`]: skip to its end.
    skipping: bool,
    event: String,
    data: Vec<String>,
}

impl EventParser {
    /// A parser at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `chunk` and returns the events it completes.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<HubEvent> {
        let mut out = Vec::new();
        let mut rest = chunk;
        while let Some(at) = rest.iter().position(|&b| b == b'\n') {
            let (part, after) = rest.split_at(at);
            rest = &after[1..];
            if self.skipping {
                self.skipping = false;
                self.line.clear();
                continue;
            }
            self.line.extend_from_slice(part);
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.field(&line, &mut out);
        }
        if !self.skipping {
            self.line.extend_from_slice(rest);
            if self.line.len() > MAX_LINE {
                self.line.clear();
                self.skipping = true;
            }
        }
        out
    }

    fn field(&mut self, line: &[u8], out: &mut Vec<HubEvent>) {
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            if !self.data.is_empty() || !event.is_empty() {
                out.push(HubEvent {
                    event: if event.is_empty() {
                        "message".to_owned()
                    } else {
                        event
                    },
                    data: std::mem::take(&mut self.data).join("\n"),
                });
            }
            return;
        }
        if line[0] == b':' {
            return; // a comment, such as `: ping`
        }
        let line = String::from_utf8_lossy(line);
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_ref(), ""),
        };
        match name {
            "event" => self.event = value.to_owned(),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
    }
}

// Natively: the tests block on the transport's futures.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::block_on;

    /// Answers each request with the next scripted response and records it.
    #[derive(Clone, Default)]
    struct Script {
        sent: Rc<RefCell<Vec<HttpRequest>>>,
        answers: Rc<RefCell<Vec<HttpResponse>>>,
    }

    impl Script {
        fn answer(&self, status: u16, headers: &[(&str, &str)], body: &[u8]) {
            self.answers.borrow_mut().push(HttpResponse {
                status,
                headers: headers
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                body: body.to_vec(),
            });
        }
        fn last(&self) -> HttpRequest {
            self.sent.borrow().last().cloned().expect("a request")
        }
        fn header(&self, name: &str) -> Option<String> {
            self.last()
                .headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    impl HttpClient for Script {
        fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>> {
            self.sent.borrow_mut().push(request);
            let next = {
                let mut a = self.answers.borrow_mut();
                (!a.is_empty()).then(|| a.remove(0))
            };
            Box::pin(async move { next.ok_or_else(|| Error::network("no answer scripted")) })
        }
    }

    fn config() -> HubConfig {
        HubConfig {
            url: "https://hub.test/".into(),
            database: "notes".into(),
            token: "mh_secret".into(),
        }
    }

    fn replica() -> (HubReplica, Script) {
        let script = Script::default();
        (HubReplica::new(config(), script.clone()).unwrap(), script)
    }

    const KEY: &str = "ltx/0/0000000000000001-0000000000000001.ltx";

    #[test]
    fn get_reads_bytes_and_the_etag_without_quotes() {
        let (r, s) = replica();
        s.answer(200, &[("etag", "\"abc\"")], b"bytes");
        s.answer(404, &[], br#"{"message":"no such object"}"#);
        let obj = block_on(r.get(KEY)).unwrap().unwrap();
        assert_eq!(obj.bytes, b"bytes");
        assert_eq!(obj.etag, "abc");
        let req = s.last();
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.url,
            format!("https://hub.test/api/hub/db/notes/o/{KEY}")
        );
        assert_eq!(
            s.header("authorization").as_deref(),
            Some("Bearer mh_secret")
        );
        assert_eq!(block_on(r.get("moul/head.json")).unwrap(), None);
        assert!(block_on(r.get("../x")).is_err(), "keys are checked");
    }

    #[test]
    fn put_sends_the_condition_and_reads_the_new_etag() {
        let (r, s) = replica();
        s.answer(200, &[("etag", "\"e1\"")], br#"{"etag":"e1"}"#);
        assert_eq!(
            block_on(r.put("moul/head.json", b"{}", Condition::IfNoneMatch)).unwrap(),
            Put::Written { etag: "e1".into() }
        );
        assert_eq!(s.last().method, "PUT");
        assert_eq!(s.last().body, b"{}");
        assert_eq!(s.header("if-none-match").as_deref(), Some("*"));
        assert_eq!(s.header("if-match"), None);

        // Without the header (a hub that does not expose it to pages), the
        // body says it.
        s.answer(200, &[], br#"{"etag":"\"e2\""}"#);
        assert_eq!(
            block_on(r.put("moul/head.json", b"{}", Condition::IfMatch("e1".into()))).unwrap(),
            Put::Written { etag: "e2".into() }
        );
        assert_eq!(s.header("if-match").as_deref(), Some("\"e1\""));
        assert_eq!(s.header("if-none-match"), None);

        s.answer(412, &[], br#"{"message":"precondition failed"}"#);
        assert_eq!(
            block_on(r.put(KEY, b"x", Condition::IfNoneMatch)).unwrap(),
            Put::Refused
        );
        s.answer(200, &[("etag", "W/\"e3\"")], b"");
        assert_eq!(
            block_on(r.put("files/abc", b"x", Condition::None)).unwrap(),
            Put::Written { etag: "e3".into() }
        );
        assert_eq!(s.header("if-match"), None);
        assert_eq!(s.header("if-none-match"), None);
    }

    #[test]
    fn list_reads_items_in_key_order() {
        let (r, s) = replica();
        s.answer(
            200,
            &[("content-type", "application/json")],
            br#"{"items":[
                {"key":"ltx/1/0000000000000001-0000000000000004.ltx","size":40,"modified":1763000000000},
                {"key":"ltx/0/0000000000000005-0000000000000005.ltx","size":7,"modified":"2025-11-13T02:13:20Z"}
            ]}"#,
        );
        let items = block_on(r.list("ltx/")).unwrap();
        assert_eq!(
            s.last().url,
            "https://hub.test/api/hub/db/notes/list?prefix=ltx%2F"
        );
        assert_eq!(items.len(), 2);
        assert!(items[0].key.starts_with("ltx/0/"));
        assert_eq!(items[0].size, 7);
        assert_eq!(items[0].modified, 1_763_000_000_000);
        assert_eq!(items[1].modified, 1_763_000_000_000);

        s.answer(200, &[], br#"{"items":[]}"#);
        assert!(block_on(r.list("")).unwrap().is_empty());
        assert_eq!(
            s.last().url,
            "https://hub.test/api/hub/db/notes/list?prefix="
        );

        s.answer(200, &[], b"<html>");
        assert_eq!(
            block_on(r.list("ltx/")).unwrap_err().kind(),
            ErrorKind::Replica
        );
    }

    #[test]
    fn delete_succeeds_on_204() {
        let (r, s) = replica();
        s.answer(204, &[], b"");
        block_on(r.delete(KEY)).unwrap();
        assert_eq!(s.last().method, "DELETE");
        assert_eq!(
            s.last().url,
            format!("https://hub.test/api/hub/db/notes/o/{KEY}")
        );
    }

    #[test]
    fn errors_say_whether_to_retry_and_never_carry_the_token() {
        let (r, s) = replica();
        for (status, kind) in [
            (401, ErrorKind::Denied),
            (403, ErrorKind::Denied),
            (429, ErrorKind::Network),
            (500, ErrorKind::Network),
            (503, ErrorKind::Network),
            (413, ErrorKind::Invalid),
            (404, ErrorKind::NotFound),
        ] {
            s.answer(status, &[], br#"{"message":"the hub said no"}"#);
            let e = block_on(r.put(KEY, b"x", Condition::IfNoneMatch)).unwrap_err();
            assert_eq!(e.kind(), kind, "{status}");
            assert_eq!(e.is_transient(), kind == ErrorKind::Network, "{status}");
            let text = e.to_string();
            assert!(text.contains("the hub said no"), "{text}");
            assert!(text.contains("moul+https://hub.test/notes"), "{text}");
            assert!(!text.contains("mh_secret"), "{text}");
        }
        s.answer(401, &[], b"");
        let e = block_on(r.get(KEY)).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Denied);
        assert!(!e.is_transient());
        // No answer at all is a network error.
        assert_eq!(block_on(r.get(KEY)).unwrap_err().kind(), ErrorKind::Network);
    }

    #[test]
    fn describes_itself_without_the_token() {
        let (r, _) = replica();
        assert_eq!(r.describe(), "moul+https://hub.test/notes");
        assert!(r.maintained_remotely());
        assert_eq!(r.events_url(), "https://hub.test/api/hub/db/notes/events");
        assert!(!format!("{r:?}").contains("mh_secret"));
        assert!(!format!("{:?}", config()).contains("mh_secret"));

        let back = HubConfig::from_url(&r.describe(), "mh_secret").unwrap();
        assert_eq!(back.url, "https://hub.test");
        assert_eq!(back.database, "notes");
        let under = HubConfig::from_url("moul+http://localhost:8091/sync/team-1", "mh_t").unwrap();
        assert_eq!(under.url, "http://localhost:8091/sync");
        assert_eq!(under.database, "team-1");
        let r = HubReplica::new(under, Script::default()).unwrap();
        assert_eq!(r.describe(), "moul+http://localhost:8091/sync/team-1");
        assert_eq!(
            r.events_url(),
            "http://localhost:8091/sync/api/hub/db/team-1/events"
        );

        for bad in [
            "https://hub.test/notes",
            "moul+https://hub.test",
            "moul+https://hub.test/",
            "moul+ftp://hub.test/notes",
            "moul+https://hub.test/Notes",
            "moul+https://hub.test/-notes",
            "moul+https://user@hub.test/notes",
        ] {
            assert!(HubConfig::from_url(bad, "mh_t").is_err(), "{bad}");
        }
        assert!(HubConfig::from_url("moul+https://hub.test/notes", "").is_err());
    }

    #[test]
    fn database_names() {
        for good in ["a", "notes", "0", "team-1", &"a".repeat(63)] {
            assert!(valid_database_name(good), "{good}");
        }
        for bad in ["", "-a", "A", "a_b", "a.b", "a/b", "ä", &"a".repeat(64)] {
            assert!(!valid_database_name(bad), "{bad}");
        }
    }

    #[test]
    fn join_tickets() {
        let (r, s) = replica();
        s.answer(
            201,
            &[],
            br#"{"ticket":"tk_1","expiresAt":"2025-11-13T02:13:20Z"}"#,
        );
        let t = block_on(r.create_join_ticket()).unwrap();
        assert_eq!(t.ticket, "tk_1");
        assert_eq!(t.expires_at, 1_763_000_000_000);
        assert_eq!(s.last().method, "POST");
        assert_eq!(
            s.last().url,
            "https://hub.test/api/hub/databases/notes/joins"
        );
        assert!(!format!("{t:?}").contains("tk_1"));
        s.answer(403, &[], br#"{"message":"account tokens only"}"#);
        let e = block_on(r.create_join_ticket()).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Denied);
        assert!(e.to_string().contains("account tokens only"));
    }

    #[test]
    fn events_come_whole_however_the_stream_is_cut() {
        let stream = b"event: connected\ndata: {\"database\":\"notes\"}\n\n: ping\n\n\
                       event: head\r\ndata: {\"txid\":7}\r\n\r\n\
                       data: one\ndata:two\nid: 3\nretry: 10\n\n";
        let want = vec![
            HubEvent {
                event: "connected".into(),
                data: "{\"database\":\"notes\"}".into(),
            },
            HubEvent {
                event: "head".into(),
                data: "{\"txid\":7}".into(),
            },
            HubEvent {
                event: "message".into(),
                data: "one\ntwo".into(),
            },
        ];
        for size in [1, 2, 3, 7, stream.len()] {
            let mut p = EventParser::new();
            let got: Vec<HubEvent> = stream.chunks(size).flat_map(|c| p.push(c)).collect();
            assert_eq!(got, want, "chunks of {size}");
        }
        // An unfinished event waits for its blank line.
        let mut p = EventParser::new();
        assert!(p.push(b"event: head\ndata: {}\n").is_empty());
        assert_eq!(p.push(b"\n").len(), 1);
        // A line past the limit is dropped, and the stream goes on.
        let mut p = EventParser::new();
        assert!(p.push(&vec![b'x'; MAX_LINE + 10]).is_empty());
        assert!(p.push(b"yyy\n\nevent: head\ndata: 1\n\n").len() == 1);
    }
}
