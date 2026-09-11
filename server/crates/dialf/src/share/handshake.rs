//! The authentication handshake that guards a shared endpoint.
//!
//! The ADB server protocol has no authentication of its own — a loopback bind *is* its
//! security model — so anything that exposes it to a network has to supply the gate. A
//! connection must prove it holds the shared token before a single byte is spliced through.
//!
//! Challenge-response rather than "send the token": the server issues a random nonce and the
//! client answers `HMAC-SHA256(token, nonce)`, so the token never crosses the wire and a
//! captured answer can't be replayed against a later connection.
//!
//! This buys authentication, **not** confidentiality — the proxied session that follows is
//! plaintext. Off-LAN use belongs inside a VPN or SSH tunnel. See `AGENTS.md`.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

/// Protocol banner; the version guards against a future format change.
pub const MAGIC: &str = "DIALF-SHARE/1";
/// Nonce length in bytes (hex-encoded on the wire, so 64 characters).
pub const NONCE_BYTES: usize = 32;
/// Shortest token we will accept. Below this a token is guessable enough that offering it as
/// a security control would be a lie.
pub const MIN_TOKEN_LEN: usize = 16;
/// Longest preamble line we will read, so a peer can't make us buffer without bound.
pub const MAX_LINE: usize = 256;

/// Tokens that look like a placeholder someone forgot to replace.
const PLACEHOLDERS: [&str; 4] = ["change-me", "changeme", "secret", "token"];

/// Why a token is unusable. Checked before binding, so a misconfigured share never listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Missing,
    TooShort(usize),
    Placeholder,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Missing => write!(
                f,
                "no share token set — generate one with `dialf adb share new-token` \
                 and put it in `adb_share.token`"
            ),
            TokenError::TooShort(n) => write!(
                f,
                "share token is {n} chars; {MIN_TOKEN_LEN}+ required \
                 (`dialf adb share new-token` prints a good one)"
            ),
            TokenError::Placeholder => write!(
                f,
                "share token is a placeholder — replace it with a real secret \
                 (`dialf adb share new-token`)"
            ),
        }
    }
}

impl std::error::Error for TokenError {}

/// Reject a token that would make the gate decorative. Callers run this *before* binding:
/// refusing to listen is the only safe response to a misconfigured secret.
pub fn check_token(token: &str) -> Result<(), TokenError> {
    let t = token.trim();
    if t.is_empty() {
        return Err(TokenError::Missing);
    }
    if PLACEHOLDERS.iter().any(|p| t.eq_ignore_ascii_case(p)) {
        return Err(TokenError::Placeholder);
    }
    if t.chars().count() < MIN_TOKEN_LEN {
        return Err(TokenError::TooShort(t.chars().count()));
    }
    Ok(())
}

/// A fresh random nonce, hex-encoded — the challenge half of the handshake.
pub fn new_nonce() -> String {
    let mut buf = [0u8; NONCE_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    hex(&buf)
}

/// A fresh random token for `adb_share.token` (same entropy as a nonce).
pub fn new_token() -> String {
    new_nonce()
}

/// The answer a client holding `token` must give for `nonce`.
pub fn expected_auth(token: &str, nonce: &str) -> String {
    let mut mac = <Hmac<Sha256>>::new_from_slice(token.trim().as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(nonce.as_bytes());
    hex(&mac.finalize().into_bytes())
}

/// Whether `answer` is the right response for `nonce` under `token`.
///
/// Compared through `Mac::verify_slice`, which is constant-time — a byte-wise `==` here would
/// leak the prefix length of a correct answer to a peer who can time us.
pub fn verify(token: &str, nonce: &str, answer: &str) -> bool {
    let Some(bytes) = unhex(answer.trim()) else {
        return false;
    };
    let mut mac = <Hmac<Sha256>>::new_from_slice(token.trim().as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(nonce.as_bytes());
    mac.verify_slice(&bytes).is_ok()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || s.is_empty() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_is_deterministic_per_token_and_nonce() {
        let a = expected_auth("s3cret-token-value", "abcd");
        assert_eq!(a, expected_auth("s3cret-token-value", "abcd"));
        assert_ne!(a, expected_auth("s3cret-token-value", "abce")); // nonce matters
        assert_ne!(a, expected_auth("other-token-value!", "abcd")); // token matters
        assert_eq!(a.len(), 64); // SHA-256, hex
    }

    #[test]
    fn verify_accepts_only_the_right_answer() {
        let nonce = new_nonce();
        let token = "the-real-share-token";
        assert!(verify(token, &nonce, &expected_auth(token, &nonce)));
        assert!(!verify("wrong-token-entirely", &nonce, &expected_auth(token, &nonce)));
        assert!(!verify(token, "a-different-nonce", &expected_auth(token, &nonce)));
    }

    #[test]
    fn verify_rejects_malformed_answers() {
        // A truncated or non-hex answer must fail closed, not panic or slice out of bounds.
        let nonce = new_nonce();
        let token = "the-real-share-token";
        let good = expected_auth(token, &nonce);
        assert!(!verify(token, &nonce, ""));
        assert!(!verify(token, &nonce, &good[..32])); // truncated
        assert!(!verify(token, &nonce, &good[..63])); // odd length
        assert!(!verify(token, &nonce, "zz".repeat(32).as_str())); // not hex
        assert!(!verify(token, &nonce, &format!("{good}00"))); // too long
    }

    #[test]
    fn answers_are_trimmed_of_transport_whitespace() {
        // The wire is line-based, so a stray \r from a peer must not fail a valid answer.
        let nonce = new_nonce();
        let token = "the-real-share-token";
        let good = expected_auth(token, &nonce);
        assert!(verify(token, &nonce, &format!("{good}\r")));
        assert!(verify(token, &nonce, &format!("  {good}  ")));
    }

    #[test]
    fn nonces_do_not_repeat() {
        let a = new_nonce();
        assert_eq!(a.len(), NONCE_BYTES * 2);
        assert_ne!(a, new_nonce());
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn weak_tokens_are_refused_before_we_ever_listen() {
        assert_eq!(check_token(""), Err(TokenError::Missing));
        assert_eq!(check_token("   "), Err(TokenError::Missing));
        assert_eq!(check_token("change-me"), Err(TokenError::Placeholder));
        assert_eq!(check_token("CHANGE-ME"), Err(TokenError::Placeholder));
        assert_eq!(check_token("short"), Err(TokenError::TooShort(5)));
        assert!(check_token("a-perfectly-fine-token").is_ok());
        assert!(check_token(&new_token()).is_ok());
    }

    #[test]
    fn generated_tokens_pass_their_own_check() {
        // Guards against new_token() drifting below MIN_TOKEN_LEN.
        for _ in 0..5 {
            assert!(check_token(&new_token()).is_ok());
        }
    }
}
