//! Signing in with GitHub, Google or Apple: the half that needs the
//! network. The engine makes the addresses and checks the state
//! (`moul_core::oauth`); the server trades the provider's code for the
//! person's profile and hands it to the engine, which signs them in.
//!
//! | Path | |
//! | --- | --- |
//! | `GET`, `POST /api/moul/:name/oauth2/:provider/callback` | where the provider sends the person back, with `code` and the `state` the engine made; answers `{token, record}` |
//! | `POST /api/moul/:name/auth-with-oauth2` | `{provider, code, codeVerifier, redirectUrl}` from an app that sent the person to the provider itself |

use axum::extract::{Path, Request, State};
use axum::response::Response;
use moul_core::ExternalProfile;
use moul_core::oauth::{ProviderConfig, callback_url, provider_config};
use serde_json::{Value, json};

use crate::http::{AppState, ClientIp, error, to_http};

fn form_or_query(query: Option<&str>, body: &[u8], content_type: &str) -> Vec<(String, String)> {
    let mut out = moul_core::api::parse_query(query.unwrap_or(""));
    if content_type.starts_with("application/x-www-form-urlencoded") {
        out.extend(moul_core::api::parse_query(&String::from_utf8_lossy(body)));
    } else if content_type.starts_with("application/json")
        && let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body)
    {
        for (k, v) in m {
            let v = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            out.push((k, v));
        }
    }
    out
}

fn first(pairs: &[(String, String)], keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| {
            pairs
                .iter()
                .find(|(pk, v)| pk == k && !v.is_empty())
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_default()
}

/// `GET|POST /api/moul/:name/oauth2/:provider/callback`
pub(crate) async fn callback(
    State(state): State<AppState>,
    Path((name, provider)): Path<(String, String)>,
    req: Request,
) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0.to_string());
    let query = req.uri().query().map(str::to_owned);
    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let pairs = form_or_query(query.as_deref(), &body, &content_type);
    let provider_error = first(&pairs, &["error"]);
    if !provider_error.is_empty() {
        let detail = first(&pairs, &["error_description"]);
        return error(
            400,
            format!("The provider refused the sign-in: {provider_error} {detail}").trim(),
        );
    }
    let code = first(&pairs, &["code"]);
    if code.is_empty() {
        return error(400, "Missing code parameter");
    }
    let state_token = first(&pairs, &["state"]);
    let (n, p) = (name.clone(), provider.clone());
    let public = state.config.public_url();
    let looked = state
        .db
        .run(move |db| {
            let s = db.engine.verify_oauth_state(&state_token);
            let conn = db.engine.store().conn();
            let config = provider_config(conn, &p);
            let redirect = callback_url(conn, &public, &n, &p.to_ascii_lowercase());
            (s, config, redirect)
        })
        .await;
    let Ok((verified, config, redirect)) = looked else {
        return error(503, "The database is closed");
    };
    let Some(verified) =
        verified.filter(|s| s.collection == name && s.provider.eq_ignore_ascii_case(&provider))
    else {
        return error(
            400,
            "Invalid or expired OAuth state; start the sign-in again",
        );
    };
    let config = match config {
        Ok(c) => c,
        Err(e) => return error(400, &e),
    };
    let user = first(&pairs, &["user"]);
    finish(
        &state,
        &name,
        &config,
        &code,
        Some(&verified.verifier),
        &redirect,
        &user,
        ip,
    )
    .await
}

/// `POST /api/moul/:name/auth-with-oauth2`
pub(crate) async fn with_oauth2(
    State(state): State<AppState>,
    Path(name): Path<String>,
    req: Request,
) -> Response {
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0.to_string());
    let body = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let Ok(b) = serde_json::from_slice::<Value>(&body) else {
        return error(400, "Invalid request body");
    };
    let text = |k: &str| b[k].as_str().unwrap_or("").trim().to_owned();
    let (provider, code) = (text("provider"), text("code"));
    if provider.is_empty() || code.is_empty() {
        return error(400, "provider and code are required");
    }
    let (n, p) = (name.clone(), provider.clone());
    let public = state.config.public_url();
    let looked = state
        .db
        .run(move |db| {
            let conn = db.engine.store().conn();
            (
                provider_config(conn, &p),
                callback_url(conn, &public, &n, &p.to_ascii_lowercase()),
            )
        })
        .await;
    let Ok((config, default_redirect)) = looked else {
        return error(503, "The database is closed");
    };
    let config = match config {
        Ok(c) => c,
        Err(e) => return error(400, &e),
    };
    let redirect = Some(text("redirectUrl"))
        .filter(|s| !s.is_empty())
        .unwrap_or(default_redirect);
    let verifier = text("codeVerifier");
    let user = match &b["user"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let verifier = (!verifier.is_empty()).then_some(verifier);
    finish(
        &state,
        &name,
        &config,
        &code,
        verifier.as_deref(),
        &redirect,
        &user,
        ip,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish(
    state: &AppState,
    collection: &str,
    config: &ProviderConfig,
    code: &str,
    verifier: Option<&str>,
    redirect: &str,
    apple_user: &str,
    ip: Option<String>,
) -> Response {
    let profile = match exchange(
        &state.outbound,
        config,
        code,
        verifier,
        redirect,
        apple_user,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return error(400, &format!("OAuth token exchange failed: {e}")),
    };
    let c = collection.to_owned();
    match state
        .db
        .run(move |db| db.engine.sign_in_external(&c, profile, ip.as_deref()))
        .await
    {
        Ok(r) => to_http(r, None),
        Err(e) => axum::response::IntoResponse::into_response(e),
    }
}

async fn json_of(
    r: Result<reqwest::Response, reqwest::Error>,
    what: &str,
) -> Result<Value, String> {
    let r = r.map_err(|e| format!("{what}: {e}"))?;
    let status = r.status();
    let bytes = r.bytes().await.map_err(|e| format!("{what}: {e}"))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("{what}: {e}"))?;
    if !status.is_success() || v.get("error").is_some_and(|e| !e.is_null()) {
        let e = v["error_description"]
            .as_str()
            .or(v["error"].as_str())
            .or(v["message"].as_str())
            .unwrap_or("refused");
        return Err(format!("{what}: {e}"));
    }
    Ok(v)
}

fn token_form(
    config: &ProviderConfig,
    secret: &str,
    code: &str,
    verifier: Option<&str>,
    redirect: &str,
) -> String {
    let mut pairs = vec![
        ("client_id", config.client_id.as_str()),
        ("client_secret", secret),
        ("code", code),
        ("redirect_uri", redirect),
        ("grant_type", "authorization_code"),
    ];
    if let Some(v) = verifier {
        pairs.push(("code_verifier", v));
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", moul_core::oauth::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Trades `code` for the person's profile.
pub async fn exchange(
    client: &reqwest::Client,
    config: &ProviderConfig,
    code: &str,
    verifier: Option<&str>,
    redirect: &str,
    apple_user: &str,
) -> Result<ExternalProfile, String> {
    let p = config.provider;
    let secret = if p.name == "apple" {
        apple_secret(config)?
    } else {
        config.client_secret.clone()
    };
    let token = json_of(
        client
            .post(p.token_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(token_form(config, &secret, code, verifier, redirect))
            .send()
            .await,
        "token",
    )
    .await?;
    let access = token["access_token"].as_str().unwrap_or("").to_owned();
    let s = |v: &Value| v.as_str().unwrap_or("").to_owned();
    match p.name {
        "github" => {
            let user = json_of(
                client
                    .get(p.profile_url)
                    .bearer_auth(&access)
                    .header("User-Agent", "moul")
                    .send()
                    .await,
                "profile",
            )
            .await?;
            let emails = json_of(
                client
                    .get("https://api.github.com/user/emails")
                    .bearer_auth(&access)
                    .header("User-Agent", "moul")
                    .send()
                    .await,
                "emails",
            )
            .await
            .unwrap_or(Value::Null);
            let list = emails.as_array().cloned().unwrap_or_default();
            let public = s(&user["email"]);
            // The public email if GitHub verified it, else the verified
            // primary one; an unverified address is never vouched for.
            let pick = list
                .iter()
                .find(|e| {
                    !public.is_empty() && e["email"] == public.as_str() && e["verified"] == true
                })
                .or_else(|| {
                    list.iter()
                        .find(|e| e["primary"] == true && e["verified"] == true)
                })
                .or_else(|| list.iter().find(|e| e["verified"] == true));
            let (email, verified) = match pick {
                Some(e) => (s(&e["email"]), true),
                None => (public, false),
            };
            let login = s(&user["login"]);
            Ok(ExternalProfile {
                provider: "github".into(),
                id: match &user["id"] {
                    Value::Number(n) => n.to_string(),
                    other => s(other),
                },
                email,
                email_verified: verified,
                name: Some(s(&user["name"]))
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| login.clone()),
                username: login,
                avatar_url: s(&user["avatar_url"]),
            })
        }
        "google" => {
            let u = json_of(
                client.get(p.profile_url).bearer_auth(&access).send().await,
                "profile",
            )
            .await?;
            let email = s(&u["email"]);
            Ok(ExternalProfile {
                provider: "google".into(),
                id: s(&u["sub"]),
                email_verified: u["email_verified"] == true || u["email_verified"] == "true",
                username: email.split('@').next().unwrap_or("").to_owned(),
                email,
                name: s(&u["name"]),
                avatar_url: s(&u["picture"]),
            })
        }
        "apple" => {
            // The id token came straight from Apple over TLS, in answer to
            // this server's own request.
            let claims = token["id_token"]
                .as_str()
                .and_then(|t| t.split('.').nth(1))
                .and_then(|p| {
                    moul_core::crypto::base64_decode(&p.replace('-', "+").replace('_', "/"))
                })
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .ok_or("Apple sent no id token")?;
            let user: Value = serde_json::from_str(apple_user).unwrap_or(Value::Null);
            let name = format!(
                "{} {}",
                s(&user["name"]["firstName"]),
                s(&user["name"]["lastName"])
            )
            .trim()
            .to_owned();
            let mut email = s(&claims["email"]);
            if email.is_empty() {
                email = s(&user["email"]);
            }
            let sub = s(&claims["sub"]);
            Ok(ExternalProfile {
                provider: "apple".into(),
                email_verified: claims["email_verified"] == true
                    || claims["email_verified"] == "true",
                username: if !name.is_empty() {
                    name.clone()
                } else if !email.is_empty() {
                    email.split('@').next().unwrap_or("").to_owned()
                } else {
                    format!("apple_{}", sub.chars().take(8).collect::<String>())
                },
                id: sub,
                email,
                name,
                avatar_url: String::new(),
            })
        }
        other => Err(format!("unsupported oauth provider: {other}")),
    }
}

/// Apple's client secret: a ready-made JWT from the settings, or one made
/// now from the team id, key id and private key, valid five minutes.
fn apple_secret(config: &ProviderConfig) -> Result<String, String> {
    let stored = config.client_secret.trim();
    if stored.split('.').count() == 3 {
        return Ok(stored.to_owned());
    }
    let pem = config
        .apple_key
        .as_ref()
        .map(|(_, _, p)| p.clone())
        .filter(|p| p.contains("PRIVATE KEY"))
        .or_else(|| stored.contains("PRIVATE KEY").then(|| stored.to_owned()));
    let (Some(pem), Some((team, key_id, _))) = (pem, config.apple_key.as_ref()) else {
        if stored.is_empty() {
            return Err(
                "Apple needs a client secret, or its team id, key id and private key".into(),
            );
        }
        return Ok(stored.to_owned());
    };
    use p256::ecdsa::signature::Signer;
    use p256::pkcs8::DecodePrivateKey;
    let key = p256::ecdsa::SigningKey::from_pkcs8_pem(&pem)
        .map_err(|e| format!("Apple private key: {e}"))?;
    let now = moul_vfs::system_clock() / 1000;
    let b64 = |v: &Value| moul_core::crypto::base64url(v.to_string().as_bytes());
    let head = b64(&json!({"alg": "ES256", "kid": key_id}));
    let body = b64(&json!({
        "iss": team, "iat": now, "exp": now + 300,
        "aud": "https://appleid.apple.com", "sub": config.client_id,
    }));
    let signing_input = format!("{head}.{body}");
    let sig: p256::ecdsa::Signature = key.sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        moul_core::crypto::base64url(&sig.to_bytes())
    ))
}
