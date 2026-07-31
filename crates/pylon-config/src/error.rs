//! The crate's error type. Currently only covers config loading (Phase 1);
//! grows a request/serving variant once the hyper server itself lands.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not locate pylon.toml in {0} or any parent directory")]
    TomlNotFound(std::path::PathBuf),
    #[error("failed to read {path}: {source}")]
    Io { path: std::path::PathBuf, source: std::io::Error },
    #[error("failed to parse {path}: {source}")]
    TomlParse { path: std::path::PathBuf, source: toml::de::Error },
    #[error("pylon.toml: required section [{0}] is missing or invalid")]
    MissingSection(&'static str),
    #[error("pylon.toml: [{section}] requires '{field}'")]
    MissingField { section: &'static str, field: &'static str },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
