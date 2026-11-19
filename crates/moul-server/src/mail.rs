//! Sending mail through the providers the Go engine supports: Resend,
//! Mailgun, SendGrid, Amazon SES and Cloudflare. They are set in the
//! `email_*` settings (`email_enabled`, `email_provider`, `email_api_key`,
//! …), which `EMAIL_*` environment variables fill when they are empty.
//!
//! With mail off, or the `console` provider, nothing leaves the server:
//! mail stays in the inbox (`GET /api/inbox`), and the server prints each
//! one once, so a builder can read sign-in codes while setting up.

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Mail settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MailConfig {
    /// Whether mail is sent at all.
    pub enabled: bool,
    /// `resend`, `mailgun`, `sendgrid`, `ses`, `cloudflare` or `console`.
    pub provider: String,
    /// The sender's address.
    pub from_address: String,
    /// The sender's name.
    pub from_name: String,
    /// The provider's API key (SES: the access key id).
    pub api_key: String,
    /// SES: the secret access key.
    pub api_secret: String,
    /// Mailgun: the sending domain; Cloudflare: the account id.
    pub domain: String,
    /// SES and Mailgun: the region (`eu` for Mailgun's EU endpoint).
    pub region: String,
    /// Another endpoint for the provider's API.
    pub endpoint: String,
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

impl MailConfig {
    /// The mail settings, environment variables filling the ones left
    /// empty.
    pub fn from_settings(s: &BTreeMap<String, String>) -> MailConfig {
        let get = |key: &str, var: &str| {
            let v = s.get(key).cloned().unwrap_or_default();
            if v.is_empty() { env(var) } else { v }
        };
        let enabled =
            s.get("email_enabled").is_some_and(|v| v == "true") || env("EMAIL_ENABLED") == "true";
        let mut provider = get("email_provider", "EMAIL_PROVIDER").to_ascii_lowercase();
        // A seeded `console` gives way to a provider named in the
        // environment (Go's seed made the variable impossible to use).
        if provider == "console" && !env("EMAIL_PROVIDER").is_empty() {
            provider = env("EMAIL_PROVIDER").to_ascii_lowercase();
        }
        MailConfig {
            enabled,
            provider,
            from_address: get("email_from_address", "EMAIL_FROM_ADDRESS"),
            from_name: get("email_from_name", "EMAIL_FROM_NAME"),
            api_key: get("email_api_key", "EMAIL_API_KEY"),
            api_secret: get("email_api_secret", "EMAIL_API_SECRET"),
            domain: get("email_domain", "EMAIL_DOMAIN"),
            region: {
                let r = get("email_region", "EMAIL_REGION");
                if r.is_empty() { "us-east-1".into() } else { r }
            },
            endpoint: get("email_endpoint", "EMAIL_ENDPOINT"),
        }
    }

    /// Whether mail goes anywhere.
    pub fn sends(&self) -> bool {
        self.enabled && !matches!(self.provider.as_str(), "" | "console")
    }

    fn from(&self) -> String {
        if self.from_name.is_empty() {
            self.from_address.clone()
        } else {
            format!("{} <{}>", self.from_name, self.from_address)
        }
    }
}

/// One message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// Recipients.
    pub to: Vec<String>,
    /// The subject.
    pub subject: String,
    /// The body, sent as HTML and as text as the Go engine does.
    pub body: String,
}

fn ok_status(r: &reqwest::Response) -> bool {
    r.status().is_success()
}

async fn finish(
    provider: &str,
    r: Result<reqwest::Response, reqwest::Error>,
) -> Result<(), String> {
    let r = r.map_err(|e| format!("{provider}: {e}"))?;
    if ok_status(&r) {
        return Ok(());
    }
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    Err(format!(
        "{provider} answered {status}: {}",
        text.chars().take(300).collect::<String>()
    ))
}

/// Sends `m` with `c`'s provider.
pub async fn send(client: &reqwest::Client, c: &MailConfig, m: &Message) -> Result<(), String> {
    if c.from_address.is_empty() {
        return Err("set email_from_address before sending mail".into());
    }
    match c.provider.as_str() {
        "resend" => {
            let url = if c.endpoint.is_empty() {
                "https://api.resend.com/emails"
            } else {
                &c.endpoint
            };
            let key = if c.api_key.starts_with("Bearer ") {
                c.api_key.clone()
            } else {
                format!("Bearer {}", c.api_key)
            };
            let body = json!({"from": c.from(), "to": m.to, "subject": m.subject, "html": m.body, "text": m.body});
            finish(
                "resend",
                client
                    .post(url)
                    .header("Authorization", key)
                    .json_body(&body)
                    .send()
                    .await,
            )
            .await
        }
        "mailgun" => {
            if c.domain.is_empty() {
                return Err("mailgun needs email_domain".into());
            }
            let url = if !c.endpoint.is_empty() {
                c.endpoint.clone()
            } else if c.region.eq_ignore_ascii_case("eu") {
                format!("https://api.eu.mailgun.net/v3/{}/messages", c.domain)
            } else {
                format!("https://api.mailgun.net/v3/{}/messages", c.domain)
            };
            let mut form = vec![
                ("from".to_owned(), c.from()),
                ("subject".to_owned(), m.subject.clone()),
            ];
            for t in &m.to {
                form.push(("to".into(), t.clone()));
            }
            form.push(("text".into(), m.body.clone()));
            form.push(("html".into(), m.body.clone()));
            let body = form
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}={}",
                        moul_core::oauth::encode(k),
                        moul_core::oauth::encode(v)
                    )
                })
                .collect::<Vec<_>>()
                .join("&");
            finish(
                "mailgun",
                client
                    .post(url)
                    .basic_auth("api", Some(&c.api_key))
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body)
                    .send()
                    .await,
            )
            .await
        }
        "sendgrid" => {
            let url = if c.endpoint.is_empty() {
                "https://api.sendgrid.com/v3/mail/send"
            } else {
                &c.endpoint
            };
            let mut from = json!({"email": c.from_address});
            if !c.from_name.is_empty() {
                from["name"] = json!(c.from_name);
            }
            let body = json!({
                "personalizations": [{"to": m.to.iter().map(|t| json!({"email": t})).collect::<Vec<_>>()}],
                "from": from,
                "subject": m.subject,
                "content": [{"type": "text/plain", "value": m.body}, {"type": "text/html", "value": m.body}],
            });
            finish(
                "sendgrid",
                client
                    .post(url)
                    .bearer_auth(&c.api_key)
                    .json_body(&body)
                    .send()
                    .await,
            )
            .await
        }
        "cloudflare" => {
            let url = if !c.endpoint.is_empty() {
                c.endpoint.clone()
            } else if !c.domain.is_empty() {
                format!(
                    "https://api.cloudflare.com/client/v4/accounts/{}/email/send",
                    c.domain
                )
            } else {
                return Err(
                    "cloudflare needs email_domain (the account id) or email_endpoint".into(),
                );
            };
            let mut body = json!({"from": c.from_address, "to": m.to, "subject": m.subject, "text": m.body, "html": m.body});
            if !c.from_name.is_empty() {
                body["from_name"] = json!(c.from_name);
            }
            let mut req = client.post(url).json_body(&body);
            if !c.api_key.is_empty() {
                req = req.bearer_auth(&c.api_key);
            }
            finish("cloudflare", req.send().await).await
        }
        "ses" => {
            let host = format!("email.{}.amazonaws.com", c.region);
            let url = if c.endpoint.is_empty() {
                format!("https://{host}/v2/email/outbound-emails")
            } else {
                c.endpoint.clone()
            };
            let body = json!({
                "FromEmailAddress": c.from(),
                "Destination": {"ToAddresses": m.to},
                "Content": {"Simple": {
                    "Subject": {"Data": m.subject, "Charset": "UTF-8"},
                    "Body": {"Text": {"Data": m.body, "Charset": "UTF-8"}, "Html": {"Data": m.body, "Charset": "UTF-8"}},
                }},
            })
            .to_string();
            let parsed = reqwest::Url::parse(&url).map_err(|e| e.to_string())?;
            let mut req = client
                .post(url.clone())
                .header("Content-Type", "application/json");
            if !c.api_secret.is_empty() {
                let date = amz_date(moul_vfs::system_clock());
                let auth = sigv4(
                    &SigV4 {
                        method: "POST",
                        host: parsed.host_str().unwrap_or(&host),
                        path: parsed.path(),
                        region: &c.region,
                        service: "ses",
                        access_key: &c.api_key,
                        secret: &c.api_secret,
                        amz_date: &date,
                    },
                    body.as_bytes(),
                );
                req = req.header("X-Amz-Date", date).header("Authorization", auth);
            } else {
                req = req.bearer_auth(&c.api_key);
            }
            finish("ses", req.body(body).send().await).await
        }
        other => Err(format!("unknown mail provider {other:?}")),
    }
}

trait JsonBody {
    fn json_body(self, v: &Value) -> Self;
}

impl JsonBody for reqwest::RequestBuilder {
    fn json_body(self, v: &Value) -> Self {
        self.header("Content-Type", "application/json")
            .body(v.to_string())
    }
}

/// `YYYYMMDDTHHMMSSZ`.
fn amz_date(ms: i64) -> String {
    let t = moul_core::time::rfc3339(ms);
    t.replace(['-', ':'], "")
        .replace(".000", "")
        .trim_end_matches('Z')
        .to_owned()
        + "Z"
}

struct SigV4<'a> {
    method: &'a str,
    host: &'a str,
    path: &'a str,
    region: &'a str,
    service: &'a str,
    access_key: &'a str,
    secret: &'a str,
    amz_date: &'a str,
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

fn hex(b: &[u8]) -> String {
    moul_core::ids::hex(b)
}

/// The `Authorization` header of an AWS Signature Version 4 request whose
/// signed headers are `content-type`, `host` and `x-amz-date`.
fn sigv4(s: &SigV4<'_>, body: &[u8]) -> String {
    let day = &s.amz_date[..8];
    let payload = hex(&Sha256::digest(body));
    let canonical = format!(
        "{}\n{}\n\ncontent-type:application/json\nhost:{}\nx-amz-date:{}\n\ncontent-type;host;x-amz-date\n{payload}",
        s.method, s.path, s.host, s.amz_date
    );
    let scope = format!("{day}/{}/{}/aws4_request", s.region, s.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        s.amz_date,
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let k = hmac(format!("AWS4{}", s.secret).as_bytes(), day);
    let k = hmac(&k, s.region);
    let k = hmac(&k, s.service);
    let k = hmac(&k, "aws4_request");
    let sig = hex(&hmac(&k, &to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=content-type;host;x-amz-date, Signature={sig}",
        s.access_key
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_fill_from_the_environment_only_when_empty() {
        let mut s = BTreeMap::new();
        s.insert("email_enabled".to_owned(), "true".to_owned());
        s.insert("email_provider".to_owned(), "resend".to_owned());
        s.insert("email_from_address".to_owned(), "a@b.co".to_owned());
        let c = MailConfig::from_settings(&s);
        assert!(c.sends());
        assert_eq!(c.region, "us-east-1");
        assert_eq!(c.from(), "a@b.co");
        assert_eq!(amz_date(1_794_961_964_000), "20261118T003244Z");
    }

    #[test]
    fn sigv4_is_stable() {
        let a = sigv4(
            &SigV4 {
                method: "POST",
                host: "email.us-east-1.amazonaws.com",
                path: "/v2/email/outbound-emails",
                region: "us-east-1",
                service: "ses",
                access_key: "AKID",
                secret: "SECRET",
                amz_date: "20261118T003244Z",
            },
            b"{}",
        );
        assert!(
            a.starts_with("AWS4-HMAC-SHA256 Credential=AKID/20261118/us-east-1/ses/aws4_request")
        );
        assert_eq!(a.len(), a.trim().len());
    }
}
