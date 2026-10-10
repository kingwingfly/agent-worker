//! Session tokens: the only credential an agent container ever holds.
//!
//! A token names one session and an expiry and is signed with a secret that never leaves
//! the Worker. The Worker checks it on `/llm` and `/mcp` and swaps in the real upstream
//! credentials, so a leaked token is worth at most the remainder of one session.
//!
//! Format: `v1.<base64url(session)>.<expiry unix seconds>.<base64url(hmac-sha256)>`.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const VERSION: &str = "v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// Durable Object name of the session, e.g. `tg:12345`.
    pub session: String,
    /// Unix seconds after which the token is rejected.
    pub expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed session token",
            Self::BadSignature => "session token signature mismatch",
            Self::Expired => "session token expired",
        })
    }
}

impl std::error::Error for TokenError {}

fn mac(secret: &[u8], payload: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    mac
}

pub fn sign(secret: &[u8], claims: &Claims) -> String {
    let payload = format!(
        "{VERSION}.{}.{}",
        URL_SAFE_NO_PAD.encode(&claims.session),
        claims.expires_at
    );
    let signature = URL_SAFE_NO_PAD.encode(mac(secret, &payload).finalize().into_bytes());
    format!("{payload}.{signature}")
}

/// Checks signature and expiry. `now` is unix seconds.
pub fn verify(secret: &[u8], token: &str, now: u64) -> Result<Claims, TokenError> {
    let (payload, signature) = token.rsplit_once('.').ok_or(TokenError::Malformed)?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| TokenError::Malformed)?;
    // Constant-time comparison, and done before the payload is parsed.
    mac(secret, payload)
        .verify_slice(&signature)
        .map_err(|_| TokenError::BadSignature)?;

    let mut parts = payload.split('.');
    let (Some(VERSION), Some(session), Some(expires_at), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenError::Malformed);
    };
    let session = URL_SAFE_NO_PAD
        .decode(session)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or(TokenError::Malformed)?;
    let expires_at: u64 = expires_at.parse().map_err(|_| TokenError::Malformed)?;
    if now >= expires_at {
        return Err(TokenError::Expired);
    }
    Ok(Claims {
        session,
        expires_at,
    })
}

/// Compares two secrets without stopping at the first difference.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |diff, (x, y)| diff | (x ^ y))
            == 0
}

/// Extracts the token from an `Authorization: Bearer ...` or `x-api-key` header value.
pub fn from_header(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .unwrap_or(value)
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret";

    fn claims() -> Claims {
        Claims {
            session: "tg:-100123.456".into(),
            expires_at: 2_000,
        }
    }

    #[test]
    fn round_trips() {
        let token = sign(SECRET, &claims());
        assert_eq!(verify(SECRET, &token, 1_999), Ok(claims()));
    }

    #[test]
    fn rejects_expired() {
        let token = sign(SECRET, &claims());
        assert_eq!(verify(SECRET, &token, 2_000), Err(TokenError::Expired));
    }

    #[test]
    fn rejects_other_secret() {
        let token = sign(b"other", &claims());
        assert_eq!(verify(SECRET, &token, 0), Err(TokenError::BadSignature));
    }

    #[test]
    fn rejects_tampered_payload() {
        let token = sign(SECRET, &claims());
        let forged = token.replacen(".2000.", ".9999999999.", 1);
        assert_ne!(forged, token);
        assert_eq!(verify(SECRET, &forged, 0), Err(TokenError::BadSignature));
    }

    #[test]
    fn rejects_garbage() {
        for token in ["", "v1", "a.b.c.d", "v1..."] {
            assert!(verify(SECRET, token, 0).is_err(), "{token:?}");
        }
    }

    #[test]
    fn compares_whole_secrets() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("secret", "secret-and-more"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn strips_bearer_prefix() {
        assert_eq!(from_header("Bearer abc"), "abc");
        assert_eq!(from_header(" abc "), "abc");
    }
}
