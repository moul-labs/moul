//! Signing in with an OAuth provider (GitHub, Google, Apple): what the
//! engine can do without the network. The native server makes the calls to
//! the provider and hands the profile to
//! [`Engine::sign_in_external`](crate::Engine::sign_in_external).
//!
//! | Path | Methods | |
//! | --- | --- | --- |
//! | `/api/moul/:name/auth-methods` | `GET` | the providers, each with an address to send the person to |
//! | `/api/moul/:name/oauth2/:provider` | `GET` | that address: a redirect, or `{authUrl, state}` as JSON |
//!
//! Providers are set up in the settings (`oauth_github_enabled`, …), as in
//! the Go engine. Where this departs from Go:
//!
//! - **The state is checked.** It is signed by the engine's key and names
//!   the collection, the provider and an expiry (ten minutes); the server's
//!   callback refuses any other. Go never checked it.
//! - **The callback does PKCE without storing anything.** The code verifier
//!   is derived from the state with the engine's key, so the server can
//!   recompute it when the provider sends the person back; the state in the
//!   URL does not reveal it.
//! - **An account is linked by email only when the provider vouches for
//!   the address.** Go linked any profile to any record with the same
//!   email, verified or not.

use serde_json::{Map, Value, json};

use crate::api::{ApiError, ApiResult, Response};
use crate::engine::{Cx, derive_key};
use crate::{crypto, settings};

/// How long a sign-in that went to a provider may take to come back.
pub const STATE_TTL_MS: i64 = 10 * 60 * 1000;

/// A provider's fixed addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Provider {
    /// `github`, `google` or `apple`.
    pub name: &'static str,
    /// For people: `GitHub`, …
    pub title: &'static str,
    /// Where the person signs in.
    pub authorize_url: &'static str,
    /// Where a code is exchanged.
    pub token_url: &'static str,
    /// Where the profile is read, if the provider has one.
    pub profile_url: &'static str,
    /// The scopes asked for.
    pub scopes: &'static str,
}

/// The providers, as the Go engine knows them.
pub const PROVIDERS: [Provider; 3] = [
    Provider {
        name: "github",
        title: "GitHub",
        authorize_url: "https://github.com/login/oauth/authorize",
        token_url: "https://github.com/login/oauth/access_token",
        profile_url: "https://api.github.com/user",
        scopes: "user:email",
    },
    Provider {
        name: "google",
        title: "Google",
        authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
        token_url: "https://oauth2.googleapis.com/token",
        profile_url: "https://www.googleapis.com/oauth2/v3/userinfo",
        scopes: "openid profile email",
    },
    Provider {
        name: "apple",
        title: "Apple",
        authorize_url: "https://appleid.apple.com/auth/authorize",
        token_url: "https://appleid.apple.com/auth/token",
        profile_url: "",
        scopes: "name email",
    },
];

/// A provider set up and switched on in the settings: its fixed addresses
/// and the app's credentials there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderConfig {
    /// The provider.
    pub provider: Provider,
    /// The app's client id.
    pub client_id: String,
    /// The client secret, or for Apple a ready-made secret JWT; empty for
    /// Apple when the key below is used instead.
    pub client_secret: String,
    /// Apple: the team id, key id and private key (PEM) that mint a secret.
    pub apple_key: Option<(String, String, String)>,
}

/// The provider `name` names, if it is enabled and has credentials.
pub fn provider_config(conn: &rusqlite::Connection, name: &str) -> Result<ProviderConfig, String> {
    let name = name.trim().to_ascii_lowercase();
    let Some(provider) = PROVIDERS.iter().find(|p| p.name == name).copied() else {
        return Err(format!("unsupported oauth provider: {name}"));
    };
    let get = |k: &str| settings::get(conn, &format!("oauth_{name}_{k}"));
    if get("enabled") != "true" {
        return Err(format!("{name} oauth provider is not enabled"));
    }
    let client_id = get("client_id");
    let client_secret = get("client_secret");
    if client_id.is_empty() {
        return Err(format!("{name} oauth provider is missing its client id"));
    }
    let apple_key = (name == "apple").then(|| (get("team_id"), get("key_id"), get("private_key")));
    let apple_key = apple_key.filter(|(t, k, p)| !t.is_empty() && !k.is_empty() && !p.is_empty());
    if client_secret.is_empty() && apple_key.is_none() {
        return Err(format!(
            "{name} oauth provider is missing its client secret"
        ));
    }
    Ok(ProviderConfig {
        provider,
        client_id,
        client_secret,
        apple_key,
    })
}

/// Where the provider sends the person back: the `oauth_redirect_url`
/// setting as the base, else the server's public address.
pub fn callback_url(
    conn: &rusqlite::Connection,
    public_url: &str,
    collection: &str,
    provider: &str,
) -> String {
    let base = settings::get(conn, "oauth_redirect_url");
    let base = if base.trim().is_empty() {
        public_url.to_owned()
    } else {
        base
    };
    format!(
        "{}/api/moul/{collection}/oauth2/{provider}/callback",
        base.trim().trim_end_matches('/')
    )
}

/// What a signed state says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    /// The auth collection.
    pub collection: String,
    /// The provider.
    pub provider: String,
    /// The PKCE verifier that goes with it.
    pub verifier: String,
}

fn verifier_for(key: &[u8], payload: &str) -> String {
    crypto::base64url(&crypto::hmac_sha256(
        &derive_key(key, "oauth-pkce"),
        payload.as_bytes(),
    ))
}

/// Makes a state for a sign-in with `provider` into `collection`: the
/// token to send, and what it says, with the PKCE verifier that goes with
/// it.
pub(crate) fn new_state(cx: &mut Cx<'_>, collection: &str, provider: &str) -> (String, State) {
    let nonce = crate::ids::random_chars(cx.host, 16);
    let payload = crypto::base64url(
        json!({"c": collection, "p": provider, "e": cx.now_ms + STATE_TTL_MS, "n": nonce})
            .to_string()
            .as_bytes(),
    );
    let sig = crypto::base64url(&crypto::hmac_sha256(
        &derive_key(cx.key, "oauth-state"),
        payload.as_bytes(),
    ));
    let state = State {
        collection: collection.to_owned(),
        provider: provider.to_owned(),
        verifier: verifier_for(cx.key, &payload),
    };
    (format!("{payload}.{sig}"), state)
}

/// Checks a state the engine made and returns what it says, if its
/// signature holds and it has not expired.
pub fn verify_state(key: &[u8], state: &str, now_ms: i64) -> Option<State> {
    let (payload, sig) = state.split_once('.')?;
    let want = crypto::base64url(&crypto::hmac_sha256(
        &derive_key(key, "oauth-state"),
        payload.as_bytes(),
    ));
    if !crypto::constant_time_eq(want.as_bytes(), sig.as_bytes()) {
        return None;
    }
    let raw = crypto::base64_decode(&payload.replace('-', "+").replace('_', "/"))?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    if v["e"].as_i64()? < now_ms {
        return None;
    }
    Some(State {
        collection: v["c"].as_str()?.to_owned(),
        provider: v["p"].as_str()?.to_owned(),
        verifier: verifier_for(key, payload),
    })
}

/// The S256 challenge of a verifier.
pub fn challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    crypto::base64url(&Sha256::digest(verifier.as_bytes()))
}

fn query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes a query value.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The address that starts a sign-in with `p`.
pub fn auth_url(
    p: &ProviderConfig,
    redirect: &str,
    state: &str,
    challenge: Option<&str>,
) -> String {
    let mut q: Vec<(&str, &str)> = vec![
        ("client_id", &p.client_id),
        ("redirect_uri", redirect),
        ("scope", p.provider.scopes),
        ("state", state),
    ];
    match p.provider.name {
        "google" => q.push(("response_type", "code")),
        "apple" => {
            q.push(("response_type", "code id_token"));
            q.push(("response_mode", "form_post"));
        }
        _ => {}
    }
    if let Some(c) = challenge {
        q.push(("code_challenge", c));
        q.push(("code_challenge_method", "S256"));
    }
    format!("{}?{}", p.provider.authorize_url, query(&q))
}

fn auth_collection(cx: &mut Cx<'_>, name: &str) -> ApiResult<moul_store::schema::Collection> {
    let c = cx
        .collection(name)?
        .ok_or_else(|| ApiError::not_found("Not found"))?;
    if c.kind != moul_store::schema::CollectionKind::Auth {
        return Err(ApiError::bad_request("This moul is not an auth collection"));
    }
    Ok(c)
}

/// `GET /api/moul/:name/auth-methods`
pub(crate) fn methods(cx: &mut Cx<'_>, name: &str) -> ApiResult {
    let c = auth_collection(cx, name)?;
    let mut providers = Vec::new();
    for p in PROVIDERS {
        let Ok(config) = provider_config(cx.conn, p.name) else {
            continue;
        };
        let (token, state) = new_state(cx, &c.name, p.name);
        let redirect = callback_url(cx.conn, &cx.config.public_url, &c.name, p.name);
        let ch = challenge(&state.verifier);
        providers.push(json!({
            "name": p.name,
            "title": p.title,
            "authUrl": auth_url(&config, &redirect, &token, Some(&ch)),
            "state": token,
            "codeVerifier": state.verifier,
            "codeChallenge": ch,
        }));
    }
    Ok(Response::json(
        200,
        json!({
            "password": {"enabled": true},
            "otp": {"enabled": true},
            "passkeys": {"enabled": false},
            "oauth2": {"enabled": !providers.is_empty(), "providers": providers},
        }),
    ))
}

/// `GET /api/moul/:name/oauth2/:provider`: sends the person to the
/// provider, or with `Accept: application/json` (or `?json=true`) answers
/// `{authUrl, state}`. `redirectUrl` (or `redirect_uri`) names another
/// address to come back to, for an app that finishes the sign-in itself
/// with `auth-with-oauth2`; it then also passes `codeChallenge`.
pub(crate) fn authorize(cx: &mut Cx<'_>, name: &str, provider: &str) -> ApiResult {
    let c = auth_collection(cx, name)?;
    let config = provider_config(cx.conn, provider).map_err(ApiError::bad_request)?;
    let q = |k: &str| cx.req.query_param(k).to_owned();
    let own_redirect = [q("redirectUrl"), q("redirect_uri")]
        .into_iter()
        .find(|s| !s.is_empty());
    let (token, state) = new_state(cx, &c.name, config.provider.name);
    let (redirect, ch) = match own_redirect {
        Some(r) => {
            let given = [q("codeChallenge"), q("code_challenge")]
                .into_iter()
                .find(|s| !s.is_empty());
            (r, given)
        }
        None => (
            callback_url(
                cx.conn,
                &cx.config.public_url,
                &c.name,
                config.provider.name,
            ),
            Some(challenge(&state.verifier)),
        ),
    };
    let url = auth_url(&config, &redirect, &token, ch.as_deref());
    let wants_json = cx.req.header_value("Accept").contains("application/json")
        || cx.req.query_param("json") == "true";
    if wants_json {
        return Ok(Response::json(200, json!({"authUrl": url, "state": token})));
    }
    Ok(Response::empty(302).with_header("Location", &url))
}

/// A person as a provider describes them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExternalProfile {
    /// The provider: `github`, `google`, `apple`.
    pub provider: String,
    /// Their id at the provider.
    pub id: String,
    /// Their email, if the provider gave one.
    pub email: String,
    /// Whether the provider says the email is theirs.
    pub email_verified: bool,
    /// A display name.
    pub name: String,
    /// A username to start from.
    pub username: String,
    /// A picture.
    pub avatar_url: String,
}

impl ExternalProfile {
    /// The entry kept in the account's `oauthProviders`.
    pub(crate) fn link(&self) -> Value {
        let mut m = Map::new();
        m.insert("provider".into(), json!(self.provider));
        m.insert("id".into(), json!(self.id));
        if !self.email.is_empty() {
            m.insert("email".into(), json!(self.email));
        }
        if !self.name.is_empty() {
            m.insert("name".into(), json!(self.name));
        }
        if !self.avatar_url.is_empty() {
            m.insert("avatarUrl".into(), json!(self.avatar_url));
        }
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_verify_and_expire() {
        let key = b"k".repeat(32);
        let payload = crypto::base64url(
            json!({"c": "users", "p": "github", "e": 1000, "n": "x"})
                .to_string()
                .as_bytes(),
        );
        let sig = crypto::base64url(&crypto::hmac_sha256(
            &derive_key(&key, "oauth-state"),
            payload.as_bytes(),
        ));
        let token = format!("{payload}.{sig}");
        let s = verify_state(&key, &token, 999).unwrap();
        assert_eq!(
            (s.collection.as_str(), s.provider.as_str()),
            ("users", "github")
        );
        assert_eq!(s.verifier.len(), 43);
        assert!(verify_state(&key, &token, 1001).is_none(), "expired");
        assert!(
            verify_state(&b"j".repeat(32), &token, 999).is_none(),
            "another key"
        );
        assert!(verify_state(&key, &format!("{payload}.x{sig}"), 999).is_none());
        // RFC 7636's example.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
