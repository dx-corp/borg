//! Reads configuration files written as TOML or JSON. The two are equivalent:
//! the same keys, defaults and validation apply to either.
//!
//! A `.toml` or `.json` extension picks the format. Any other name is read as
//! JSON when its first non-blank character is `{`, and as TOML otherwise.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
}

impl Format {
    pub fn of(path: &Path, text: &str) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("toml") => Self::Toml,
            Some(e) if e.eq_ignore_ascii_case("json") => Self::Json,
            _ if text.trim_start().starts_with('{') => Self::Json,
            _ => Self::Toml,
        }
    }
}

/// A parse failure, naming the file and the place in it. TOML errors carry
/// the line, column and offending key; JSON errors the line and column.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("{path}: {source}")]
    Toml {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("{path}:{line}:{column}: {source}")]
    Json {
        path: PathBuf,
        line: usize,
        column: usize,
        source: serde_json::Error,
    },
}

/// Parses `text`, read from `path`, in the format [`Format::of`] picks.
pub fn parse<T: DeserializeOwned>(path: &Path, text: &str) -> Result<T, ParseError> {
    match Format::of(path, text) {
        Format::Toml => toml::from_str(text).map_err(|source| ParseError::Toml {
            path: path.to_owned(),
            source: Box::new(source),
        }),
        Format::Json => serde_json::from_str(text).map_err(|source| ParseError::Json {
            path: path.to_owned(),
            line: source.line(),
            column: source.column(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        name: String,
        #[serde(default)]
        count: u32,
    }

    #[test]
    fn extension_wins_then_content() {
        assert_eq!(Format::of(Path::new("a.toml"), "{"), Format::Toml);
        assert_eq!(Format::of(Path::new("a.JSON"), "x = 1"), Format::Json);
        assert_eq!(
            Format::of(Path::new("config"), "  \n{\"a\":1}"),
            Format::Json
        );
        assert_eq!(Format::of(Path::new("config"), "a = 1"), Format::Toml);
        assert_eq!(Format::of(Path::new("config"), ""), Format::Toml);
    }

    #[test]
    fn both_formats_parse_to_the_same_value() {
        let t: Sample = parse(Path::new("s.toml"), "name = \"x\"\ncount = 2\n").unwrap();
        let j: Sample = parse(Path::new("s.json"), r#"{"name":"x","count":2}"#).unwrap();
        assert_eq!(t, j);
    }

    #[test]
    fn errors_name_the_file_and_place() {
        let e = parse::<Sample>(Path::new("/etc/borg/q.toml"), "name = \"x\"\nnope = 1\n")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("/etc/borg/q.toml: "), "{e}");
        assert!(e.contains("line 2"), "{e}");
        assert!(e.contains("nope"), "{e}");
        let e = parse::<Sample>(Path::new("/etc/borg/q.json"), "{\n\"nope\":1}")
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("/etc/borg/q.json:2:"), "{e}");
    }
}
