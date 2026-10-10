//! A string that never prints itself.

use serde::{Deserialize, Serialize};

/// Secret text (installation tokens, JIT runner configs). `Debug` is
/// redacted; serialization is transparent because the value must reach the
/// one party entitled to it (GitHub or the leased drone).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted_but_serde_is_transparent() {
        let s = Secret::new("hunter2");
        assert!(!format!("{s:?}").contains("hunter2"));
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"hunter2\"");
        assert_eq!(
            serde_json::from_str::<Secret>("\"x\"").unwrap().expose(),
            "x"
        );
    }
}
