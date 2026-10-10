//! Drone enrollment tokens. The queen stores only the SHA-256 of a token, so a
//! database leak does not let anyone impersonate a drone. Tokens are 256 bits
//! of OS randomness, which makes an unsalted fast hash sufficient.

use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

const PREFIX: &str = "borg_";

/// A fresh random enrollment token. Shown once to the operator.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("{PREFIX}{}", hex::encode(bytes))
}

/// The value stored and looked up for `token`.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// A drone name is part of runner names and labels, so keep it to a safe set.
pub fn valid_drone_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_hash_deterministically() {
        let (a, b) = (new_token(), new_token());
        assert_ne!(a, b);
        assert!(a.starts_with("borg_") && a.len() == 5 + 64);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
        assert!(!hash_token(&a).contains(&a));
    }

    #[test]
    fn drone_names_are_restricted() {
        assert!(valid_drone_name("host-1"));
        for bad in ["", "-x", "Host", "a b", "a_b", &"x".repeat(41)] {
            assert!(!valid_drone_name(bad), "{bad}");
        }
    }
}
