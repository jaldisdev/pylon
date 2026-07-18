#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Pgcon(#[from] pylon_pgcon::Error),
    #[error("cache error: {0}")]
    Cache(String),
}

impl From<Box<dyn std::error::Error + Send + Sync>> for Error {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Error::Cache(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
