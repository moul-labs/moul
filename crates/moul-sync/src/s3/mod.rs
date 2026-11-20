//! A replica in an S3-compatible bucket: Amazon S3, Cloudflare R2, RustFS.
//!
//! Requests are signed with SigV4 and sent through an [`HttpClient`]. Writes
//! that must not race carry `If-None-Match: *` (a new TXID) or `If-Match`
//! (the writer and head records); S3 and R2 evaluate both atomically and
//! answer 412 when the condition fails.

mod http;
mod sigv4;
mod xml;

#[cfg(target_arch = "wasm32")]
pub use http::FetchClient;
#[cfg(not(target_arch = "wasm32"))]
pub use http::UreqClient;
pub use http::{HttpClient, HttpRequest, HttpResponse};

use crate::replica::{BoxFuture, Condition, Object, ObjectInfo, Put, Replica, check_key};
use crate::time::{amz_date, parse_rfc3339};
use crate::{Error, ErrorKind, Result};

/// Where a bucket is and how to sign for it.
#[derive(Clone, PartialEq, Eq)]
pub struct S3Config {
    /// The service endpoint, such as `https://<account>.r2.cloudflarestorage.com`,
    /// `https://s3.us-east-1.amazonaws.com` or `http://localhost:9000`.
    pub endpoint: String,
    /// The signing region: `auto` for R2, `us-east-1` for RustFS.
    pub region: String,
    /// The bucket.
    pub bucket: String,
    /// A folder inside the bucket for this replica, without leading or
    /// trailing `/`. May be empty.
    pub prefix: String,
    /// The access key ID.
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: String,
    /// A session token, for temporary credentials.
    pub session_token: Option<String>,
    /// Address the bucket as a path (`endpoint/bucket/key`) rather than as a
    /// host name (`bucket.endpoint/key`). R2 and RustFS accept paths.
    pub path_style: bool,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &self.access_key_id)
            .field("path_style", &self.path_style)
            .finish_non_exhaustive()
    }
}

impl S3Config {
    /// Reads `s3://bucket/prefix?endpoint=…&region=…&path-style=false`. The
    /// endpoint defaults to Amazon S3 in the region, and the region to
    /// `us-east-1`, or `auto` for an R2 endpoint.
    pub fn from_url(url: &str, access_key_id: &str, secret_access_key: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("s3://")
            .ok_or_else(|| Error::invalid(format!("{url:?} is not an s3:// URL")))?;
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        let (bucket, prefix) = (percent_decode(bucket, false), percent_decode(prefix, false));
        if bucket.is_empty() {
            return Err(Error::invalid(format!("{url:?} names no bucket")));
        }
        let mut endpoint = None;
        let mut region = None;
        let mut path_style = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = percent_decode(v, true);
            match k {
                "endpoint" => endpoint = Some(v),
                "region" => region = Some(v),
                "path-style" | "path_style" | "forcePathStyle" => {
                    path_style = Some(!matches!(v.as_str(), "false" | "0" | "no"))
                }
                other => return Err(Error::invalid(format!("unknown S3 option {other:?}"))),
            }
        }
        let region = region.unwrap_or_else(|| {
            if endpoint
                .as_deref()
                .is_some_and(|e| e.contains("r2.cloudflarestorage.com"))
            {
                "auto".into()
            } else {
                "us-east-1".into()
            }
        });
        let custom = endpoint.is_some();
        let endpoint = endpoint.unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
        Ok(Self {
            endpoint,
            region,
            bucket,
            prefix: prefix.trim_matches('/').to_owned(),
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            session_token: None,
            path_style: path_style.unwrap_or(custom),
        })
    }
}

/// Decodes `%XX`, and `+` as a space when `plus` (in a query string).
fn percent_decode(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(h) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(if plus && b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A replica in an S3-compatible bucket.
pub struct S3Replica {
    config: S3Config,
    client: Box<dyn HttpClient>,
    clock: Box<dyn Fn() -> i64>,
    scheme: String,
    authority: String,
    base_path: String,
}

impl std::fmt::Debug for S3Replica {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Replica")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl S3Replica {
    /// A replica in the bucket `config` describes, reached through `client`.
    pub fn new(config: S3Config, client: impl HttpClient + 'static) -> Result<Self> {
        let (scheme, rest) = config.endpoint.split_once("://").ok_or_else(|| {
            Error::invalid(format!("endpoint {:?} has no scheme", config.endpoint))
        })?;
        if scheme != "https" && scheme != "http" {
            return Err(Error::invalid(format!(
                "endpoint scheme {scheme:?} is not http(s)"
            )));
        }
        let (authority, base) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() || config.bucket.is_empty() {
            return Err(Error::invalid(
                "an S3 replica needs an endpoint host and a bucket",
            ));
        }
        let authority = if config.path_style {
            authority.to_owned()
        } else {
            format!("{}.{authority}", config.bucket)
        };
        let base = base.trim_matches('/');
        Ok(Self {
            scheme: scheme.to_owned(),
            authority,
            base_path: if base.is_empty() {
                String::new()
            } else {
                format!("/{base}")
            },
            config,
            client: Box::new(client),
            clock: Box::new(moul_vfs::system_clock),
        })
    }

    /// A replica on the platform's HTTP client: `ureq` natively, `fetch` in
    /// the browser.
    pub fn with_default_client(config: S3Config) -> Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        let client = UreqClient::default();
        #[cfg(target_arch = "wasm32")]
        let client = FetchClient;
        Self::new(config, client)
    }

    /// Uses `clock` for request dates, so tests can sign the same bytes.
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// The bucket key for a replica key.
    fn object_key(&self, key: &str) -> String {
        if self.config.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}/{key}", self.config.prefix)
        }
    }

    /// The encoded path for a bucket key, or for the bucket itself.
    fn path(&self, object_key: Option<&str>) -> String {
        let mut path = self.base_path.clone();
        if self.config.path_style {
            path.push('/');
            path.push_str(&sigv4::uri_encode(&self.config.bucket, false));
        }
        path.push('/');
        if let Some(k) = object_key {
            path.push_str(&sigv4::uri_encode(k, true));
        }
        path
    }

    fn request(
        &self,
        method: &'static str,
        path: String,
        query: Vec<(String, String)>,
        extra: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> HttpRequest {
        let date = amz_date((self.clock)());
        let payload_hash = sigv4::sha256_hex(&body);
        let mut headers = vec![
            ("host".to_owned(), self.authority.clone()),
            ("x-amz-content-sha256".to_owned(), payload_hash.clone()),
            ("x-amz-date".to_owned(), date.clone()),
        ];
        if let Some(token) = &self.config.session_token {
            headers.push(("x-amz-security-token".to_owned(), token.clone()));
        }
        headers.extend(extra);
        let auth = sigv4::authorization(&sigv4::Signing {
            method,
            path: &path,
            query: &query,
            headers: &headers,
            payload_hash: &payload_hash,
            amz_date: &date,
            region: &self.config.region,
            access_key_id: &self.config.access_key_id,
            secret_access_key: &self.config.secret_access_key,
        });
        headers.push(("authorization".to_owned(), auth));
        let qs = sigv4::canonical_query(&query);
        let url = format!(
            "{}://{}{path}{}{qs}",
            self.scheme,
            self.authority,
            if qs.is_empty() { "" } else { "?" }
        );
        HttpRequest {
            method,
            url,
            headers,
            body,
        }
    }

    fn failure(&self, what: &str, r: &HttpResponse) -> Error {
        let body = String::from_utf8_lossy(&r.body);
        let code = xml::first(&body, "Code").unwrap_or_default();
        let message = xml::first(&body, "Message").unwrap_or_default();
        let detail = format!(
            "{what} in {}: {} {code} {message}",
            self.describe(),
            r.status
        );
        let kind = match r.status {
            401 | 403 => ErrorKind::Denied,
            404 => ErrorKind::NotFound,
            408 | 409 | 429 | 500..=599 => ErrorKind::Network,
            _ => ErrorKind::Replica,
        };
        Error::new(kind, detail.trim_end().to_owned())
    }

    async fn get_now(&self, key: &str) -> Result<Option<Object>> {
        check_key(key)?;
        let path = self.path(Some(&self.object_key(key)));
        let r = self
            .client
            .send(self.request("GET", path, vec![], vec![], vec![]))
            .await?;
        match r.status {
            200 => Ok(Some(Object {
                etag: r.header("etag").unwrap_or_default().to_owned(),
                bytes: r.body,
            })),
            404 => Ok(None),
            _ => Err(self.failure(&format!("GET {key}"), &r)),
        }
    }

    async fn put_now(&self, key: &str, bytes: &[u8], condition: Condition) -> Result<Put> {
        check_key(key)?;
        let path = self.path(Some(&self.object_key(key)));
        let extra = match &condition {
            Condition::None => vec![],
            Condition::IfNoneMatch => vec![("if-none-match".to_owned(), "*".to_owned())],
            Condition::IfMatch(etag) => vec![("if-match".to_owned(), etag.clone())],
        };
        let r = self
            .client
            .send(self.request("PUT", path, vec![], extra, bytes.to_vec()))
            .await?;
        match r.status {
            200 | 201 | 204 => Ok(Put::Written {
                etag: r.header("etag").unwrap_or_default().to_owned(),
            }),
            412 => Ok(Put::Refused),
            // If-Match on a missing object.
            404 if matches!(condition, Condition::IfMatch(_)) => Ok(Put::Refused),
            _ => Err(self.failure(&format!("PUT {key}"), &r)),
        }
    }

    async fn list_now(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let full = self.object_key(prefix);
        let strip = if self.config.prefix.is_empty() {
            0
        } else {
            self.config.prefix.len() + 1
        };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), full.clone()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token".to_owned(), t.clone()));
            }
            let r = self
                .client
                .send(self.request("GET", self.path(None), query, vec![], vec![]))
                .await?;
            if r.status != 200 {
                return Err(self.failure(&format!("LIST {prefix}"), &r));
            }
            let body = String::from_utf8_lossy(&r.body);
            for c in xml::elements(&body, "Contents") {
                let Some(key) = xml::first(c, "Key") else {
                    continue;
                };
                let Some(key) = key.get(strip..).map(str::to_owned) else {
                    continue;
                };
                out.push(ObjectInfo {
                    key,
                    size: xml::first(c, "Size")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    modified: xml::first(c, "LastModified")
                        .and_then(|s| parse_rfc3339(&s))
                        .unwrap_or(0),
                });
            }
            token = xml::first(&body, "NextContinuationToken");
            if xml::first(&body, "IsTruncated").as_deref() != Some("true") || token.is_none() {
                break;
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn delete_now(&self, key: &str) -> Result<()> {
        check_key(key)?;
        let path = self.path(Some(&self.object_key(key)));
        let r = self
            .client
            .send(self.request("DELETE", path, vec![], vec![], vec![]))
            .await?;
        match r.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(self.failure(&format!("DELETE {key}"), &r)),
        }
    }
}

impl Replica for S3Replica {
    fn describe(&self) -> String {
        if self.config.prefix.is_empty() {
            format!("s3://{}", self.config.bucket)
        } else {
            format!("s3://{}/{}", self.config.bucket, self.config.prefix)
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let c = S3Config::from_url(
            "s3://notes/team%20a/app?endpoint=http%3A%2F%2Flocalhost%3A9000",
            "k",
            "s",
        )
        .unwrap();
        assert_eq!(c.endpoint, "http://localhost:9000");
        assert_eq!(c.prefix, "team a/app");
        assert_eq!(c.region, "us-east-1");
        assert!(c.path_style);
        let r = S3Replica::new(c, NoClient).unwrap();
        let req = r.request(
            "GET",
            r.path(Some(&r.object_key("ltx/0/x.ltx"))),
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            req.url,
            "http://localhost:9000/notes/team%20a/app/ltx/0/x.ltx"
        );

        let aws = S3Config::from_url("s3://b?region=eu-west-1", "k", "s").unwrap();
        assert_eq!(aws.endpoint, "https://s3.eu-west-1.amazonaws.com");
        assert!(!aws.path_style);
        let r = S3Replica::new(aws, NoClient).unwrap();
        let req = r.request(
            "GET",
            r.path(None),
            vec![("list-type".into(), "2".into())],
            vec![],
            vec![],
        );
        assert_eq!(req.url, "https://b.s3.eu-west-1.amazonaws.com/?list-type=2");

        let r2 = S3Config::from_url(
            "s3://b/p?endpoint=https://acct.r2.cloudflarestorage.com",
            "k",
            "s",
        )
        .unwrap();
        assert_eq!(r2.region, "auto");
        assert!(S3Config::from_url("https://b", "k", "s").is_err());
        assert!(
            format!("{r2:?}").find("\"s\"").is_none(),
            "the secret is not printed"
        );
    }

    struct NoClient;
    impl HttpClient for NoClient {
        fn send(&self, _: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>> {
            Box::pin(async { Err(Error::network("no network in this test")) })
        }
    }
}
