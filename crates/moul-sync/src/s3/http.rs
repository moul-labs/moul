//! The HTTP the S3 transport needs, behind a trait: `ureq` natively and
//! `fetch` in the browser.

use crate::replica::BoxFuture;
use crate::{Error, Result};

/// One HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// `GET`, `PUT` or `DELETE`.
    pub method: &'static str,
    /// The full URL.
    pub url: String,
    /// Headers to send. `host` is listed for signing; clients that set it
    /// themselves (every one here) leave it out.
    pub headers: Vec<(String, String)>,
    /// The body; empty for none.
    pub body: Vec<u8>,
}

/// One HTTP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// Headers, names in lower case.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Sends HTTP requests. A transport error (no answer at all) is an
/// [`ErrorKind::Network`](crate::ErrorKind::Network) error; any answer,
/// whatever its status, is a response.
pub trait HttpClient {
    /// Sends `request` and reads the whole response.
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>>;
}

/// HTTP with `ureq`, blocking inside the future.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub struct UreqClient {
    agent: ureq::Agent,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for UreqClient {
    fn default() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .into();
        Self { agent }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl UreqClient {
    fn send_now(&self, request: HttpRequest) -> Result<HttpResponse> {
        let mut builder = ureq::http::Request::builder()
            .method(request.method)
            .uri(&request.url);
        for (k, v) in &request.headers {
            if k != "host" {
                builder = builder.header(k, v);
            }
        }
        let fail = |e: &dyn std::fmt::Display| {
            Error::network(format!("{} {}: {e}", request.method, request.url))
        };
        let response = if request.method == "PUT" {
            let req = builder.body(request.body.clone()).map_err(|e| fail(&e))?;
            self.agent.run(req)
        } else {
            let req = builder.body(()).map_err(|e| fail(&e))?;
            self.agent.run(req)
        };
        let mut response = response.map_err(|e| fail(&e))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response
            .body_mut()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()
            .map_err(|e| fail(&e))?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HttpClient for UreqClient {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>> {
        Box::pin(async move { self.send_now(request) })
    }
}

/// HTTP with the browser's `fetch`, in a page or a worker. The bucket's CORS
/// rule must allow the page's origin, the methods `GET`, `PUT` and `DELETE`,
/// the request headers sent here, and expose `ETag`.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Default)]
pub struct FetchClient;

#[cfg(target_arch = "wasm32")]
impl FetchClient {
    async fn send_now(request: HttpRequest) -> Result<HttpResponse> {
        use js_sys::{Array, Uint8Array};
        use wasm_bindgen::{JsCast, JsValue};
        use wasm_bindgen_futures::JsFuture;
        use web_sys::{Headers, Request, RequestInit, Response};

        let fail = |e: JsValue| {
            let detail = e
                .dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
                .unwrap_or_else(|| format!("{e:?}"));
            Error::network(format!(
                "{} {}: {detail} (if the bucket is reachable, check its CORS rule)",
                request.method, request.url
            ))
        };
        let headers = Headers::new().map_err(fail)?;
        for (k, v) in &request.headers {
            if k != "host" {
                headers.set(k, v).map_err(fail)?;
            }
        }
        let init = RequestInit::new();
        init.set_method(request.method);
        init.set_headers(&headers);
        init.set_cache(web_sys::RequestCache::NoStore);
        if !request.body.is_empty() {
            init.set_body(&Uint8Array::from(request.body.as_slice()).into());
        }
        let req = Request::new_with_str_and_init(&request.url, &init).map_err(fail)?;
        let global = js_sys::global();
        let promise = if let Some(w) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
            w.fetch_with_request(&req)
        } else if let Some(w) = global.dyn_ref::<web_sys::Window>() {
            w.fetch_with_request(&req)
        } else {
            return Err(Error::network("fetch is not available here"));
        };
        let response: Response = JsFuture::from(promise)
            .await
            .map_err(fail)?
            .unchecked_into();
        let mut out_headers = Vec::new();
        if let Ok(Some(iter)) = js_sys::try_iter(&response.headers()) {
            for entry in iter.flatten() {
                let pair: Array = entry.unchecked_into();
                if let (Some(k), Some(v)) = (pair.get(0).as_string(), pair.get(1).as_string()) {
                    out_headers.push((k.to_ascii_lowercase(), v));
                }
            }
        }
        let buf = JsFuture::from(response.array_buffer().map_err(fail)?)
            .await
            .map_err(fail)?;
        Ok(HttpResponse {
            status: response.status(),
            headers: out_headers,
            body: Uint8Array::new(&buf).to_vec(),
        })
    }
}

#[cfg(target_arch = "wasm32")]
impl HttpClient for FetchClient {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>> {
        Box::pin(Self::send_now(request))
    }
}
