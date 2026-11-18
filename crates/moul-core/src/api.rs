//! Requests and responses.
//!
//! A request has the shape of an HTTP request (method, path, query,
//! headers, body), because the same requests go to a worker in the page, to
//! the native server and to the hub, and because the Go engine's HTTP API is
//! the specification. Nothing here does I/O: a host turns its transport
//! into a [`Request`] and sends back the [`Response`].

use serde_json::{Map, Value, json};

/// Who the host vouches for, before any credentials in the headers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Caller {
    /// Nobody in particular: credentials in the headers decide.
    #[default]
    Unknown,
    /// The owner of the database, such as Studio in the builder's own tab.
    /// Bypasses rules, like the Go engine's admin key.
    Owner,
}

/// One uploaded file in a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    /// The file name the client gave.
    pub name: String,
    /// The media type the client gave; the engine also sniffs the content.
    pub content_type: String,
    /// The bytes.
    pub data: Vec<u8>,
}

/// The body of a request.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Body {
    /// No body.
    #[default]
    None,
    /// A JSON value.
    Json(Value),
    /// A body whose declared type did not parse (bad JSON, a broken
    /// multipart form). Handlers answer "Invalid request body" as the Go
    /// engine does; an import reads the bytes as Go's importer would.
    Invalid {
        /// The media type the client declared.
        content_type: String,
        /// The bytes.
        data: Vec<u8>,
    },
    /// A form with files, as `multipart/form-data` carries it.
    Form {
        /// Text fields.
        fields: Vec<(String, String)>,
        /// Files, by field name.
        files: Vec<(String, Upload)>,
    },
    /// Raw bytes with their media type, such as a CSV import.
    Bytes {
        /// The media type.
        content_type: String,
        /// The bytes.
        data: Vec<u8>,
    },
}

/// A request to the engine.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// `GET`, `POST`, `PATCH`, `PUT` or `DELETE`.
    pub method: String,
    /// The path, such as `/api/moul/posts/records`, without the query.
    pub path: String,
    /// Query parameters in order, decoded.
    pub query: Vec<(String, String)>,
    /// Headers in order. Names are compared without case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Body,
    /// What the host vouches for.
    pub caller: Caller,
    /// The request id of a write: the sending device and its counter. A
    /// write sent twice with one id is applied once. Also read from the
    /// `Moul-Request-Id` header.
    pub id: Option<String>,
    /// The client's network address as the host saw it (never a header the
    /// client sent). Limits on guessing sign-in codes count per network.
    pub remote: Option<String>,
}

impl Request {
    /// A request with no query, headers or body.
    pub fn new(method: &str, path: &str) -> Request {
        Request {
            method: method.to_ascii_uppercase(),
            path: path.to_owned(),
            query: Vec::new(),
            headers: Vec::new(),
            body: Body::None,
            caller: Caller::Unknown,
            id: None,
            remote: None,
        }
    }

    /// `GET path`.
    pub fn get(path: &str) -> Request {
        Request::new("GET", path)
    }

    /// `POST path` with a JSON body.
    pub fn post(path: &str, body: Value) -> Request {
        Request::new("POST", path).json(body)
    }

    /// `PATCH path` with a JSON body.
    pub fn patch(path: &str, body: Value) -> Request {
        Request::new("PATCH", path).json(body)
    }

    /// `DELETE path`.
    pub fn delete(path: &str) -> Request {
        Request::new("DELETE", path)
    }

    /// Splits `target` into path and query, decoding the query.
    pub fn from_target(method: &str, target: &str) -> Request {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut r = Request::new(method, path);
        r.query = parse_query(query);
        r
    }

    /// Adds a query parameter.
    pub fn query(mut self, key: &str, value: &str) -> Request {
        self.query.push((key.to_owned(), value.to_owned()));
        self
    }

    /// Adds a header.
    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Sets a JSON body.
    pub fn json(mut self, body: Value) -> Request {
        self.body = Body::Json(body);
        self
    }

    /// Sends `Authorization: Bearer <token>`.
    pub fn bearer(self, token: &str) -> Request {
        self.header("Authorization", &format!("Bearer {token}"))
    }

    /// Marks the request as the owner's.
    pub fn owner(mut self) -> Request {
        self.caller = Caller::Owner;
        self
    }

    /// Sets the request id.
    pub fn with_id(mut self, id: &str) -> Request {
        self.id = Some(id.to_owned());
        self
    }

    /// The first value of query parameter `key`, or `""`.
    pub fn query_param(&self, key: &str) -> &str {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// The first value of header `name`, compared without case, or `""`.
    pub fn header_value(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// The body as a JSON object: `Ok(None)` without a body, `Err` if the
    /// body is not an object.
    #[allow(clippy::result_unit_err)]
    pub fn json_object(&self) -> Result<Option<Map<String, Value>>, ()> {
        match &self.body {
            Body::None => Ok(None),
            Body::Json(Value::Object(m)) => Ok(Some(m.clone())),
            Body::Json(Value::Null) => Ok(None),
            Body::Form { fields, .. } => Ok(Some(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            )),
            _ => Err(()),
        }
    }
}

impl Request {
    /// Builds a request from HTTP parts, parsing the body by its
    /// `Content-Type`: JSON, `multipart/form-data`,
    /// `application/x-www-form-urlencoded`, or raw bytes for anything else.
    pub fn from_http(
        method: &str,
        path: &str,
        query: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
    ) -> Request {
        let mut r = Request::new(method, path);
        r.query = parse_query(query);
        r.headers = headers;
        let content_type = r.header_value("Content-Type").to_owned();
        let base = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        r.body = if body.is_empty() {
            Body::None
        } else if base == "application/json" || (base.is_empty() && body.first() == Some(&b'{')) {
            match serde_json::from_slice(body) {
                Ok(v) => Body::Json(v),
                Err(_) => Body::Invalid {
                    content_type: content_type.clone(),
                    data: body.to_vec(),
                },
            }
        } else if base == "multipart/form-data" {
            let boundary = content_type
                .split(';')
                .filter_map(|p| p.trim().strip_prefix("boundary="))
                .next()
                .unwrap_or("")
                .trim_matches('"')
                .to_owned();
            parse_multipart(body, &boundary).unwrap_or_else(|| Body::Invalid {
                content_type: content_type.clone(),
                data: body.to_vec(),
            })
        } else if base == "application/x-www-form-urlencoded" {
            Body::Form {
                fields: parse_query(&String::from_utf8_lossy(body)),
                files: Vec::new(),
            }
        } else {
            Body::Bytes {
                content_type,
                data: body.to_vec(),
            }
        };
        r
    }
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Parses a `multipart/form-data` body into text fields and files.
pub fn parse_multipart(body: &[u8], boundary: &str) -> Option<Body> {
    if boundary.is_empty() {
        return None;
    }
    let delim = format!("--{boundary}").into_bytes();
    let mut fields = Vec::new();
    let mut files = Vec::new();
    let mut pos = find(body, &delim, 0)? + delim.len();
    loop {
        if body.get(pos..pos + 2) == Some(b"--") {
            break;
        }
        // Skip the line break after the delimiter.
        pos = find(body, b"\r\n", pos)? + 2;
        let head_end = find(body, b"\r\n\r\n", pos)?;
        let head = String::from_utf8_lossy(&body[pos..head_end]).into_owned();
        let data_start = head_end + 4;
        let next = find(body, &delim, data_start)?;
        let mut data_end = next;
        if data_end >= data_start + 2 && &body[data_end - 2..data_end] == b"\r\n" {
            data_end -= 2;
        }
        let data = body[data_start..data_end].to_vec();
        let mut name = String::new();
        let mut filename = None;
        let mut content_type = String::new();
        for line in head.split("\r\n") {
            let (k, v) = line.split_once(':').unwrap_or((line, ""));
            if k.trim().eq_ignore_ascii_case("content-disposition") {
                for part in v.split(';').map(str::trim) {
                    if let Some(n) = part.strip_prefix("name=") {
                        name = n.trim_matches('"').to_owned();
                    } else if let Some(f) = part.strip_prefix("filename=") {
                        filename = Some(f.trim_matches('"').to_owned());
                    }
                }
            } else if k.trim().eq_ignore_ascii_case("content-type") {
                content_type = v.trim().to_owned();
            }
        }
        match filename {
            Some(f) => files.push((
                name,
                Upload {
                    name: f,
                    content_type,
                    data,
                },
            )),
            None => fields.push((name, String::from_utf8_lossy(&data).into_owned())),
        }
        pos = next + delim.len();
    }
    Some(Body::Form { fields, files })
}

/// Decodes an `application/x-www-form-urlencoded` query string.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The body of a response.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ResponseBody {
    /// No body.
    #[default]
    Empty,
    /// A JSON value.
    Json(Value),
    /// Bytes with their media type, such as a file or a CSV export.
    Bytes {
        /// The media type.
        content_type: String,
        /// The bytes.
        data: Vec<u8>,
    },
}

/// The engine's answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Headers to send.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: ResponseBody,
}

impl Response {
    /// A JSON response.
    pub fn json(status: u16, body: Value) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: ResponseBody::Json(body),
        }
    }

    /// A response without a body.
    pub fn empty(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: ResponseBody::Empty,
        }
    }

    /// A response carrying bytes.
    pub fn bytes(status: u16, content_type: &str, data: Vec<u8>) -> Response {
        Response {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: ResponseBody::Bytes {
                content_type: content_type.into(),
                data,
            },
        }
    }

    /// Adds a header.
    pub fn with_header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The JSON body, or `Null`.
    pub fn json_body(&self) -> &Value {
        static NULL: Value = Value::Null;
        match &self.body {
            ResponseBody::Json(v) => v,
            _ => &NULL,
        }
    }

    /// The `message` of an error response, or `""`.
    pub fn message(&self) -> &str {
        self.json_body()["message"].as_str().unwrap_or("")
    }

    /// The first value of header `name`, compared without case.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The size of the body as it would be sent.
    pub fn body_len(&self) -> usize {
        match &self.body {
            ResponseBody::Empty => 0,
            ResponseBody::Json(v) => serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0),
            ResponseBody::Bytes { data, .. } => data.len(),
        }
    }
}

/// An error answer: a status and a message, sent as `{"message": …}` like
/// the Go engine's errors.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    /// HTTP status code.
    pub status: u16,
    /// The message.
    pub message: String,
    /// More keys for the body, such as the spent budget.
    pub extra: Map<String, Value>,
}

impl ApiError {
    /// An error with `status` and `message`.
    pub fn new(status: u16, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
            extra: Map::new(),
        }
    }

    /// 400 Bad Request.
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(400, message)
    }

    /// 401 Unauthorized.
    pub fn unauthorized(message: impl Into<String>) -> ApiError {
        ApiError::new(401, message)
    }

    /// 403 Forbidden.
    pub fn forbidden(message: impl Into<String>) -> ApiError {
        ApiError::new(403, message)
    }

    /// 404 Not Found.
    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new(404, message)
    }

    /// 500 with the Go engine's generic message.
    pub fn internal() -> ApiError {
        ApiError::new(500, "Internal server error")
    }

    /// Adds a key to the body.
    pub fn with(mut self, key: &str, value: Value) -> ApiError {
        self.extra.insert(key.into(), value);
        self
    }

    /// The response for this error.
    pub fn into_response(self) -> Response {
        let mut body = self.extra;
        body.insert("message".into(), json!(self.message));
        Response::json(self.status, Value::Object(body))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> ApiError {
        if let rusqlite::Error::SqliteFailure(f, _) = &e
            && f.code == rusqlite::ErrorCode::OperationInterrupted
        {
            return ApiError::new(400, "The request exceeded its budget")
                .with("budget", json!("time"));
        }
        // SQLite's text can quote the statement, rules included: it is not
        // for callers.
        let _ = e;
        ApiError::internal()
    }
}

impl From<moul_store::Error> for ApiError {
    fn from(e: moul_store::Error) -> ApiError {
        match e {
            moul_store::Error::Sqlite(e) => e.into(),
            moul_store::Error::Conflict(m) => ApiError::bad_request(m),
            _ => ApiError::internal(),
        }
    }
}

/// Result type of request handlers.
pub type ApiResult<T = Response> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_decode() {
        let r = Request::from_target(
            "get",
            "/api/moul/posts/records?filter=title%20%3D%20%27a%27&perPage=2&x",
        );
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/api/moul/posts/records");
        assert_eq!(r.query_param("filter"), "title = 'a'");
        assert_eq!(r.query_param("perPage"), "2");
        assert_eq!(r.query_param("x"), "");
        assert_eq!(
            parse_query("a=1+2&b=%zz"),
            [("a".into(), "1 2".into()), ("b".into(), "%zz".into())]
        );
    }

    #[test]
    fn parses_multipart() {
        let body = b"--XYZ\r\nContent-Disposition: form-data; name=\"mode\"\r\n\r\ninsert\r\n--XYZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.csv\"\r\nContent-Type: text/csv\r\n\r\nid\r\n1\r\n--XYZ--\r\n";
        let r = Request::from_http(
            "POST",
            "/api/x",
            "",
            vec![(
                "Content-Type".into(),
                "multipart/form-data; boundary=XYZ".into(),
            )],
            body,
        );
        let Body::Form { fields, files } = r.body else {
            panic!("{:?}", r.body)
        };
        assert_eq!(fields, [("mode".to_owned(), "insert".to_owned())]);
        assert_eq!(files[0].0, "file");
        assert_eq!(files[0].1.name, "a.csv");
        assert_eq!(files[0].1.data, b"id\r\n1");
    }

    #[test]
    fn multipart_parts_may_be_empty_or_malformed() {
        for body in [
            &b"--B\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n--B--\r\n"[..],
            b"--B\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n\r\n--B--",
            b"--B\r\n\r\n--B",
            b"--B",
            b"--B\r\n",
        ] {
            let _ = parse_multipart(body, "B");
        }
        let Some(Body::Form { fields, .. }) = parse_multipart(
            b"--B\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n--B--\r\n",
            "B",
        ) else {
            panic!()
        };
        assert_eq!(fields, [("a".to_owned(), String::new())]);
    }

    #[test]
    fn errors_have_a_message() {
        let r = ApiError::not_found("Record not found").into_response();
        assert_eq!(r.status, 404);
        assert_eq!(r.json_body(), &json!({"message": "Record not found"}));
    }
}
