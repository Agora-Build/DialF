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
use rand::{Rng, RngCore};
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

/// A fresh random nonce, hex-encoded — the challenge half of the handshake.
pub fn new_nonce() -> String {
    let mut buf = [0u8; NONCE_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    hex(&buf)
}

/// Prefix on every issued share token, so one is recognisable on sight in a shell history
/// or a paste ("device share").
pub const TOKEN_PREFIX: &str = "dvs_";

/// Characters a token body is drawn from: unambiguous in a font that confuses 0/O and 1/l/I,
/// since these get copied between machines by hand.
const TOKEN_ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzACDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Length of the random part; with the prefix the token is 16 characters.
pub const TOKEN_BODY_LEN: usize = 12;

/// Mint a share token: `dvs_` plus 12 random characters.
///
/// Issued fresh for every share and kept only in memory — it is never written to config, so
/// it cannot be read back later, and a restarted share has a different secret.
pub fn new_token() -> String {
    let mut rng = rand::thread_rng();
    let body: String = (0..TOKEN_BODY_LEN)
        .map(|_| TOKEN_ALPHABET[rng.gen_range(0..TOKEN_ALPHABET.len())] as char)
        .collect();
    format!("{TOKEN_PREFIX}{body}")
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
    fn issued_tokens_have_the_documented_shape() {
        let t = new_token();
        assert!(t.starts_with(TOKEN_PREFIX), "got: {t}");
        assert_eq!(t.len(), TOKEN_PREFIX.len() + TOKEN_BODY_LEN);
        assert_eq!(t.len(), 16);
        let body = &t[TOKEN_PREFIX.len()..];
        assert!(body.chars().all(|c| TOKEN_ALPHABET.contains(&(c as u8))), "stray char in {body}");
        // Tokens are read off one screen and typed into another, so the alphabet leaves out
        // the pairs that get misread.
        for confusable in ['0', 'O', '1', 'l', 'I', 'B'] {
            assert!(
                !TOKEN_ALPHABET.contains(&(confusable as u8)),
                "{confusable} is ambiguous and should not be in the alphabet"
            );
        }
    }

    #[test]
    fn every_share_gets_a_different_token() {
        // The token is never persisted, so a repeat must not be predictable either.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            assert!(seen.insert(new_token()), "new_token repeated itself");
        }
    }

    #[test]
    fn an_issued_token_works_end_to_end_through_the_handshake() {
        let token = new_token();
        let nonce = new_nonce();
        assert!(verify(&token, &nonce, &expected_auth(&token, &nonce)));
        assert!(!verify(&new_token(), &nonce, &expected_auth(&token, &nonce)));
    }

}
