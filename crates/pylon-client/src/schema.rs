//! Loads `.pylon/schema.json` — written by `pylon.finalize()` on the Python
//! side (see `pylon/_finalize.py`) — the same artifact `pylon-lsp` already
//! consumes (`crates/pylon-lsp/src/schema.rs`) so this crate can compile
//! PyQL without an embedded Python interpreter of its own.
//!
//! Unlike the LSP (which re-`stat`s the file on every diagnostics pass to
//! pick up edits as they're saved), a `Client` loads the schema once at
//! construction — a query-serving process doesn't want an extra syscall on
//! every request — and only reloads when a caller explicitly asks via
//! `Client::reload_schema()`.

use std::path::{Path, PathBuf};

use pylon_core::schema::SchemaDescriptor;

use crate::error::{Error, Result};

pub(crate) fn default_schema_path() -> PathBuf {
    PathBuf::from(".pylon").join("schema.json")
}

pub(crate) fn load(path: &Path) -> Result<SchemaDescriptor> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Schema {
        path: path.display().to_string(),
        source,
    })?;
    Ok(serde_json::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("pylon-client-test-{tag}-{nanos}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write_schema(&self, json: &str) -> PathBuf {
            let path = self.0.join("schema.json");
            std::fs::write(&path, json).unwrap();
            path
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = ScratchDir::new("missing");
        let result = load(&dir.0.join("does-not-exist.json"));
        assert!(matches!(result, Err(Error::Schema { .. })));
    }

    #[test]
    fn loads_a_valid_schema() {
        let dir = ScratchDir::new("valid");
        let path = dir.write_schema(&serde_json::to_string(&SchemaDescriptor::default()).unwrap());
        assert!(load(&path).is_ok());
    }

    #[test]
    fn malformed_json_is_a_schema_json_error() {
        let dir = ScratchDir::new("malformed");
        let path = dir.write_schema("not valid json");
        assert!(matches!(load(&path), Err(Error::SchemaJson(_))));
    }
}
