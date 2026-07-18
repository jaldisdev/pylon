#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// A 2xx response whose JSON body didn't have the shape the provider's
    /// API is documented to return (missing field, wrong type) — distinct
    /// from `Http`, which covers transport failures and non-2xx statuses
    /// (via `reqwest::Response::error_for_status`).
    #[error("unexpected response shape from provider: {0}")]
    Shape(String),
}

pub type Result<T> = std::result::Result<T, Error>;
