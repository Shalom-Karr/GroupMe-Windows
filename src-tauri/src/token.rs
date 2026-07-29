//! Secure storage and validation of the GroupMe access token.
//!
//! The token is obtained by the webview-side injection script (inject.js),
//! which intercepts outgoing `api.groupme.com` requests and reads the
//! `x-access-token` request header — the stable wire contract, never a
//! localStorage key that can be renamed at any deploy.
//!
//! Once received, the token is persisted in Windows Credential Manager via
//! `keyring` and **never** written to the SQLite archive or any config file
//! in plaintext.  The archive stores only a SHA-256 fingerprint so the app
//! can detect when a different account has signed in and refuse to mix two
//! people's messages in one database.

use sha2::{Digest, Sha256};

pub const SERVICE: &str = "dev.shalomkarr.groupme";
pub const ACCOUNT: &str = "groupme-access-token";

// ─── Error type ────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("no token stored")]
    Missing,
    #[error("token is not a plausible GroupMe access token")]
    Malformed,
    #[error("credential store: {0}")]
    Store(String),
}

// ─── Pure helpers ───────────────────────────────────────────────────────────

/// Hex SHA-256 of the token. Safe to log, safe to persist — identifies
/// *which* token without being usable as one.
pub fn fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let bytes = hasher.finalize();
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        // infallible: writing hex digits into a String cannot fail
        let _ = write!(s, "{:02x}", b);
        s
    })
}

/// Rejects obvious junk before it reaches the credential store.
/// Real tokens are ~40 chars of [A-Za-z0-9]. Accept 20..=128 alphanumeric.
pub fn looks_like_token(candidate: &str) -> bool {
    let len = candidate.len();
    len >= 20
        && len <= 128
        && candidate.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Redacts a token for logging: first 4 chars + ellipsis. NEVER log a whole token.
pub fn redact(token: &str) -> String {
    // char_indices gives byte offsets; .nth(4) is the start of the 5th char.
    let end = token
        .char_indices()
        .nth(4)
        .map_or(token.len(), |(i, _)| i);
    format!("{}...", &token[..end])
}

// ─── TokenStore ─────────────────────────────────────────────────────────────

pub struct TokenStore {
    service: String,
    account: String,
}

impl TokenStore {
    /// Uses the app-wide [`SERVICE`] / [`ACCOUNT`] constants.
    pub fn new() -> Self {
        Self::with_account(SERVICE, ACCOUNT)
    }

    /// Constructs a store with custom names. Intended for tests so they can
    /// use an isolated credential entry without touching the production one.
    pub fn with_account(service: &str, account: &str) -> Self {
        Self {
            service: service.to_owned(),
            account: account.to_owned(),
        }
    }

    /// Builds the underlying keyring entry handle. No I/O occurs here.
    fn entry(&self) -> Result<keyring::Entry, TokenError> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| TokenError::Store(e.to_string()))
    }

    /// Validates `token` with [`looks_like_token`] then writes it to the
    /// platform credential store.
    pub fn save(&self, token: &str) -> Result<(), TokenError> {
        if !looks_like_token(token) {
            return Err(TokenError::Malformed);
        }
        let entry = self.entry()?;
        entry
            .set_password(token)
            .map_err(|e| TokenError::Store(e.to_string()))
    }

    /// Reads the token from the platform credential store.
    pub fn load(&self) -> Result<String, TokenError> {
        let entry = self.entry()?;
        match entry.get_password() {
            Ok(token) => Ok(token),
            Err(keyring::Error::NoEntry) => Err(TokenError::Missing),
            Err(e) => Err(TokenError::Store(e.to_string())),
        }
    }

    /// Removes the credential. Succeeds silently if it was already absent.
    pub fn delete(&self) -> Result<(), TokenError> {
        let entry = self.entry()?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            // Already gone — treat as success so delete is idempotent.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(TokenError::Store(e.to_string())),
        }
    }

    /// Returns `true` if a token is currently stored and retrievable.
    pub fn has_token(&self) -> bool {
        self.load().is_ok()
    }
}

impl Default for TokenStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── fingerprint ────────────────────────────────────────────────────────

    #[test]
    fn fingerprint_is_deterministic() {
        let token = "SomeGroupMeToken1234";
        assert_eq!(fingerprint(token), fingerprint(token));
    }

    #[test]
    fn fingerprint_differs_for_different_inputs() {
        assert_ne!(fingerprint("TokenAlpha"), fingerprint("TokenBeta"));
    }

    #[test]
    fn fingerprint_is_64_hex_chars() {
        let fp = fingerprint("AnyArbitraryToken");
        assert_eq!(fp.len(), 64, "SHA-256 hex is always 64 chars");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── looks_like_token ──────────────────────────────────────────────────

    #[test]
    fn looks_like_token_accepts_realistic_40_char_token() {
        // GroupMe tokens are ~40 hex/alphanumeric chars in practice.
        let token = "aB3cD4eF5gH6iJ7kL8mN9oP0qR1sT2uV3wX4yZ56";
        assert_eq!(token.len(), 40);
        assert!(looks_like_token(token));
    }

    #[test]
    fn looks_like_token_accepts_boundary_lengths() {
        assert!(looks_like_token(&"a".repeat(20)));
        assert!(looks_like_token(&"a".repeat(128)));
    }

    #[test]
    fn looks_like_token_rejects_empty() {
        assert!(!looks_like_token(""));
    }

    #[test]
    fn looks_like_token_rejects_too_short() {
        // 19 chars — one under the minimum.
        assert!(!looks_like_token("abcdefghijklmnopqrs"));
    }

    #[test]
    fn looks_like_token_rejects_too_long() {
        assert!(!looks_like_token(&"a".repeat(129)));
    }

    #[test]
    fn looks_like_token_rejects_whitespace() {
        // Space in the middle is not alphanumeric.
        let token = "aB3cD4eF5gH6iJ7kL8m N9oP0qR1sT2uV3wX4yZ5";
        assert!(!looks_like_token(token));
    }

    #[test]
    fn looks_like_token_rejects_punctuation() {
        let token = "aB3cD4eF5g-H6iJ7kL8mN9oP0qR1sT2uV3wX4yZ5";
        assert!(!looks_like_token(token));
    }

    // ── redact ────────────────────────────────────────────────────────────

    #[test]
    fn redact_never_contains_full_token() {
        let token = "aB3cD4eF5gH6iJ7kL8mN9oP0qR1sT2uV3wX4yZ56";
        let r = redact(token);
        assert!(!r.contains(token), "redacted form must not contain the full token");
    }

    #[test]
    fn redact_shows_first_four_chars_then_ellipsis() {
        let token = "aB3cD4eF5gH6iJ7kL8mN9oP0qR1sT2uV3wX4yZ56";
        assert_eq!(redact(token), "aB3c...");
    }

    #[test]
    fn redact_handles_short_token_gracefully() {
        // Even a single char should not panic.
        let r = redact("x");
        assert_eq!(r, "x...");
    }

    // ── keyring round-trip ────────────────────────────────────────────────
    //
    // Gated to Windows only (that's where Windows Credential Manager lives).
    // Tolerates a missing/locked credential store so CI without keyring
    // support doesn't fail the entire suite — if save() returns Store(_) we
    // simply skip the rest of the test rather than panicking.

    #[test]
    #[cfg(windows)]
    fn keyring_roundtrip() {
        let store = TokenStore::with_account(
            "dev.shalomkarr.groupme.test",
            "test-roundtrip-token",
        );

        // Start with a clean slate in case a previous run left debris.
        let _ = store.delete();

        let token = "TestRoundTripToken1234567890ABCDEFGHIJKx";
        assert!(looks_like_token(token), "test token must pass validation");

        match store.save(token) {
            // Credential store unavailable in this environment — skip quietly.
            Err(TokenError::Store(_)) => return,
            Err(e) => panic!("unexpected save error: {e}"),
            Ok(()) => {}
        }

        let loaded = store.load().expect("token should be loadable after save");
        assert_eq!(loaded, token);

        assert!(store.has_token());
        store.delete().expect("delete should succeed after save");
        assert!(!store.has_token());
    }

    #[test]
    #[cfg(windows)]
    fn save_rejects_malformed_token() {
        let store = TokenStore::with_account(
            "dev.shalomkarr.groupme.test",
            "test-malformed-token",
        );
        // Short / non-alphanumeric — must be rejected before touching keyring.
        assert!(matches!(store.save("bad!"), Err(TokenError::Malformed)));
    }
}
