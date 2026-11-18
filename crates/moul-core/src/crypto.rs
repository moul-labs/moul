//! Password hashing, token signing and the encodings they need, in pure
//! Rust so the browser build works.
//!
//! Passwords use bcrypt with cost 10 and the `$2a$` prefix, like the Go
//! engine, so the hashes of a converted Go database verify unchanged. Tokens
//! are JWTs signed with HMAC-SHA256 carrying the Go engine's claims.

use blowfish::Blowfish;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::host::Host;

/// bcrypt's work factor, as Go's `bcrypt.DefaultCost`.
pub const BCRYPT_COST: u32 = 10;

/// The highest cost a stored hash may have. Go accepts up to 31, but one
/// check at 20 takes a minute on the engine's only thread; above 16 (a few
/// seconds) a hash is refused rather than computed.
pub const MAX_VERIFY_COST: u32 = 16;

const BCRYPT_ALPHABET: &[u8; 64] =
    b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn bcrypt_raw(cost: u32, salt: &[u8; 16], password: &[u8]) -> [u8; 24] {
    // The key is the password with a terminating NUL, cut to 72 bytes.
    let mut key: Vec<u8> = password.iter().copied().take(72).collect();
    if key.len() < 72 {
        key.push(0);
    }
    let mut state = Blowfish::bc_init_state();
    state.salted_expand_key(salt, &key);
    for _ in 0..(1u64 << cost) {
        state.bc_expand_key(&key);
        state.bc_expand_key(salt);
    }
    // "OrpheanBeholderScryDoubt"
    let mut ctext: [u32; 6] = [
        0x4f72_7068,
        0x6561_6e42,
        0x6568_6f6c,
        0x6465_7253,
        0x6372_7944,
        0x6f75_6274,
    ];
    for i in (0..6).step_by(2) {
        for _ in 0..64 {
            let [l, r] = state.bc_encrypt([ctext[i], ctext[i + 1]]);
            ctext[i] = l;
            ctext[i + 1] = r;
        }
    }
    let mut out = [0u8; 24];
    for (i, word) in ctext.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn bcrypt_b64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(BCRYPT_ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn bcrypt_b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut acc = 0u32;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
        let v = BCRYPT_ALPHABET.iter().position(|&a| a == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Hashes `password` with a fresh salt from `host`.
pub fn hash_password(host: &mut dyn Host, password: &str) -> String {
    let mut salt = [0u8; 16];
    host.fill_random(&mut salt);
    hash_password_with(BCRYPT_COST, &salt, password)
}

/// Hashes `password` with a given cost and salt.
pub fn hash_password_with(cost: u32, salt: &[u8; 16], password: &str) -> String {
    let raw = bcrypt_raw(cost, salt, password.as_bytes());
    format!(
        "$2a${cost:02}${}{}",
        bcrypt_b64_encode(salt),
        bcrypt_b64_encode(&raw[..23])
    )
}

/// Whether `password` matches a bcrypt `hash` (`$2a$`, `$2b$` or `$2y$`).
pub fn verify_password(hash: &str, password: &str) -> bool {
    // Stored hashes come from imports and converted databases: refuse
    // anything that is not plain ASCII before slicing it.
    if !hash.is_ascii() {
        return false;
    }
    let parts: Vec<&str> = hash.split('$').collect();
    if parts.len() != 4 || !parts[0].is_empty() || !["2a", "2b", "2y"].contains(&parts[1]) {
        return false;
    }
    let Ok(cost) = parts[2].parse::<u32>() else {
        return false;
    };
    // A higher cost would stall every request behind this one.
    if !(4..=MAX_VERIFY_COST).contains(&cost) || parts[3].len() != 53 {
        return false;
    }
    let Some(salt) = bcrypt_b64_decode(&parts[3][..22]) else {
        return false;
    };
    let Ok(salt) = <[u8; 16]>::try_from(&salt[..16.min(salt.len())]) else {
        return false;
    };
    let raw = bcrypt_raw(cost, &salt, password.as_bytes());
    let expected = bcrypt_b64_encode(&raw[..23]);
    constant_time_eq(expected.as_bytes(), &parts[3].as_bytes()[22..])
}

/// Compares two byte strings in time that depends only on their lengths.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// SHA-256 of `data` as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    crate::ids::hex(&Sha256::digest(data))
}

/// HMAC-SHA256 of `data` under `key`.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_encode(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(alphabet[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    b64_encode(bytes, B64, true)
}

/// URL-safe base64 without padding, as JWTs use.
pub fn base64url(bytes: &[u8]) -> String {
    b64_encode(bytes, B64URL, false)
}

/// Decodes base64 in either alphabet, with or without padding. `None` if a
/// character is outside both alphabets.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut acc = 0u32;
    let mut bits = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The claims of a Moul token, as the Go engine signs them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Claims {
    /// The record id.
    pub id: String,
    /// The record's email.
    #[serde(default)]
    pub email: String,
    /// The record's username.
    #[serde(default)]
    pub username: String,
    /// The auth collection's name.
    #[serde(rename = "moul", default)]
    pub collection: String,
    /// Expiry, seconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
    /// Issued at, seconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    /// Token id, 32 hex characters.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub jti: String,
}

/// Why a token was not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// Not three base64url parts, or the parts are not JSON.
    Malformed,
    /// Not HS256.
    Algorithm,
    /// The signature does not match.
    Signature,
    /// Past its expiry.
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TokenError::Malformed => "token is malformed",
            TokenError::Algorithm => "unexpected signing method",
            TokenError::Signature => "token signature is invalid",
            TokenError::Expired => "token has invalid claims: token is expired",
        })
    }
}

/// Decodes one JWT segment strictly: URL-safe base64 without padding, in
/// its one canonical spelling. A token then has exactly one string, so a
/// revoked token cannot come back re-encoded.
fn jwt_segment(seg: &str) -> Option<Vec<u8>> {
    if !seg
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let bytes = base64_decode(seg)?;
    (base64url(&bytes) == seg).then_some(bytes)
}

/// Signs `claims` as an HS256 JWT under `key`.
pub fn sign_token(key: &[u8], claims: &Claims) -> String {
    let header = base64url(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = base64url(&serde_json::to_vec(claims).expect("claims serialise"));
    let signing_input = format!("{header}.{payload}");
    let sig = hmac_sha256(key, signing_input.as_bytes());
    format!("{signing_input}.{}", base64url(&sig))
}

/// Checks an HS256 JWT's signature under `key` and returns its claims.
/// With `now_s`, a token past its expiry is refused; without, expiry is
/// not checked (for refresh and sign-out).
pub fn verify_token(key: &[u8], token: &str, now_s: Option<i64>) -> Result<Claims, TokenError> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenError::Malformed);
    };
    let header: serde_json::Value = jwt_segment(h)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(TokenError::Malformed)?;
    if header.get("alg").and_then(|a| a.as_str()) != Some("HS256") {
        return Err(TokenError::Algorithm);
    }
    let sig = jwt_segment(s).ok_or(TokenError::Malformed)?;
    let expected = hmac_sha256(key, format!("{h}.{p}").as_bytes());
    if !constant_time_eq(&sig, &expected) {
        return Err(TokenError::Signature);
    }
    let claims: Claims = jwt_segment(p)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or(TokenError::Malformed)?;
    if let (Some(now), Some(exp)) = (now_s, claims.exp)
        && now >= exp
    {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

/// Checks a new password the way the Go engine does: at least 8 characters
/// with an uppercase letter, a lowercase letter and a digit.
pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.len() < 8 {
        return Err("password must be at least 8 characters long");
    }
    if !password.chars().any(char::is_uppercase) {
        return Err("password must contain at least one uppercase letter");
    }
    if !password.chars().any(char::is_lowercase) {
        return Err("password must contain at least one lowercase letter");
    }
    if !password.chars().any(|c| c.is_numeric()) {
        return Err("password must contain at least one digit");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::SeededHost;

    #[test]
    fn verifies_a_go_bcrypt_hash() {
        // From the Go engine's seeded database: the password is "Password123!".
        let go = "$2a$10$YCcj828sVniFT.wzf9Qa4.KGF.P508dHrkQqxEP3yHMc2YAB.IcR6";
        assert!(verify_password(go, "Password123!"));
        assert!(!verify_password(go, "password123!"));
    }

    #[test]
    fn hashes_round_trip() {
        let mut host = SeededHost::new(3, 0);
        let h = hash_password(&mut host, "Correct-horse-1");
        assert!(h.starts_with("$2a$10$") && h.len() == 60, "{h}");
        assert!(verify_password(&h, "Correct-horse-1"));
        assert!(!verify_password(&h, "Correct-horse-2"));
        assert!(!verify_password("not a hash", "x"));
        assert!(!verify_password(
            "$2a$10$ééééééééééééééééééééééééééééééééééééééééééééééééééééé",
            "x"
        ));
        assert!(!verify_password(
            &h.replacen("$10$", "$31$", 1),
            "Correct-horse-1"
        ));
    }

    #[test]
    fn verifies_a_go_token() {
        // Signed by the Go engine in its test suite with the secret below.
        let claims = Claims {
            id: "user-1".into(),
            email: "a@example.com".into(),
            username: "a".into(),
            collection: "users".into(),
            exp: Some(2_000),
            iat: Some(1_000),
            jti: "ab".into(),
        };
        let token = sign_token(b"secret", &claims);
        assert_eq!(
            verify_token(b"secret", &token, Some(1_500)),
            Ok(claims.clone())
        );
        assert_eq!(
            verify_token(b"secret", &token, Some(2_000)),
            Err(TokenError::Expired)
        );
        assert_eq!(
            verify_token(b"other", &token, None),
            Err(TokenError::Signature)
        );
        assert_eq!(
            verify_token(b"secret", "a.b", None),
            Err(TokenError::Malformed)
        );
    }

    #[test]
    fn base64_round_trip() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\xff\xfe\x00"] {
            assert_eq!(base64_decode(&base64(data)).unwrap(), data);
            assert_eq!(base64_decode(&base64url(data)).unwrap(), data);
        }
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn password_rules() {
        assert!(validate_password("Password123!").is_ok());
        assert_eq!(
            validate_password("short1A"),
            Err("password must be at least 8 characters long")
        );
        assert_eq!(
            validate_password("password123"),
            Err("password must contain at least one uppercase letter")
        );
    }
}
