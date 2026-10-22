use asteria_framework::UserId;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::config::ServerConfig;

const HASH_PREFIX: &str = "blake3:";

/// A fresh random bearer token: 32 random bytes as unpadded base64url (43 characters).
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("system random source unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The value stored in the server config for a token: `blake3:<hex>`.
pub fn token_hash(token: &str) -> String {
    format!("{HASH_PREFIX}{}", blake3::hash(token.as_bytes()).to_hex())
}

fn parse_hash(s: &str) -> Option<blake3::Hash> {
    s.strip_prefix(HASH_PREFIX)?.parse().ok()
}

pub(crate) fn is_valid_hash(s: &str) -> bool {
    parse_hash(s).is_some()
}

/// Verifies presented bearer tokens against the hashes in the server config.
pub struct Tokens {
    entries: Vec<(UserId, blake3::Hash)>,
}

impl Tokens {
    /// Entries whose hash is malformed can never match and are skipped.
    pub fn from_config(config: &ServerConfig) -> Tokens {
        let entries = config
            .token
            .iter()
            .filter_map(|e| Some((e.user, parse_hash(&e.hash)?)))
            .collect();
        Tokens { entries }
    }

    /// Returns the user whose stored hash matches. `blake3::Hash` equality is constant-time,
    /// and every entry is checked so timing does not reveal which one matched.
    pub fn verify(&self, presented: &str) -> Option<UserId> {
        let presented = blake3::hash(presented.as_bytes());
        let mut found = None;
        for (user, hash) in &self.entries {
            if *hash == presented {
                found = Some(*user);
            }
        }
        found
    }
}
