//! Loads `.pylon/schema.json` — written by `pylon.finalize()` on the Python
//! side (see `pylon/_finalize.py`) — so the language server can run the full
//! compiler (`pylon_core::query::compile`) instead of only the parser, and
//! can therefore surface semantic diagnostics (unknown property/link with
//! "did you mean", type mismatches) in the editor, not just syntax errors.
//!
//! There's no embedded Python interpreter in this binary to build a
//! `SchemaDescriptor` itself, so it depends on a schema-owning process
//! (`pylon.finalize()`, called by the dev server, CLI, or test setup)
//! having already exported one to disk. If no export exists yet, diagnostics
//! gracefully degrade to parser-only (matching the server's original
//! behavior) rather than failing outright.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use pylon_core::schema::SchemaDescriptor;

pub struct SchemaState {
    path: PathBuf,
    loaded_mtime: Option<SystemTime>,
    schema: Option<SchemaDescriptor>,
}

impl SchemaState {
    pub fn new(workspace_root: Option<PathBuf>) -> Self {
        let path = workspace_root
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".pylon")
            .join("schema.json");
        let mut state = Self { path, loaded_mtime: None, schema: None };
        state.reload_if_changed();
        state
    }

    /// Re-reads the schema file if its mtime advanced since the last
    /// successful load — one `stat` syscall in the common no-change case, so
    /// safe to call on every diagnostics pass rather than wiring up LSP file
    /// watchers. Clears the process-global query-compile cache on a
    /// successful reload, since it's keyed only by `(query text, session
    /// config)` and would otherwise serve results compiled against the
    /// stale schema.
    pub fn reload_if_changed(&mut self) {
        let Ok(metadata) = fs::metadata(&self.path) else { return };
        let Ok(mtime) = metadata.modified() else { return };
        if Some(mtime) == self.loaded_mtime {
            return;
        }
        let Ok(text) = fs::read_to_string(&self.path) else { return };
        match serde_json::from_str::<SchemaDescriptor>(&text) {
            Ok(schema) => {
                self.schema = Some(schema);
                self.loaded_mtime = Some(mtime);
                pylon_core::query::clear_query_cache();
                eprintln!("pylon-lsp: loaded schema from {}", self.path.display());
            }
            Err(err) => {
                eprintln!("pylon-lsp: failed to parse {}: {err}", self.path.display());
            }
        }
    }

    pub fn get(&self) -> Option<&SchemaDescriptor> {
        self.schema.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A fresh scratch dir per test, cleaned up on drop — avoids a new
    /// dependency on `tempfile` for what's just a couple of file writes.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("pylon-lsp-test-{tag}-{nanos}"));
            fs::create_dir_all(dir.join(".pylon")).unwrap();
            Self(dir)
        }

        fn write_schema(&self, json: &str) {
            fs::write(self.0.join(".pylon").join("schema.json"), json).unwrap();
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn empty_schema_json() -> String {
        serde_json::to_string(&SchemaDescriptor::default()).unwrap()
    }

    #[test]
    fn missing_schema_file_degrades_to_none() {
        let dir = ScratchDir::new("missing");
        let state = SchemaState::new(Some(dir.0.clone()));
        assert!(state.get().is_none());
    }

    #[test]
    fn loads_valid_schema_on_construction() {
        let dir = ScratchDir::new("valid");
        dir.write_schema(&empty_schema_json());
        let state = SchemaState::new(Some(dir.0.clone()));
        assert!(state.get().is_some());
    }

    #[test]
    fn malformed_schema_file_degrades_to_none_instead_of_panicking() {
        let dir = ScratchDir::new("malformed");
        dir.write_schema("not valid json");
        let state = SchemaState::new(Some(dir.0.clone()));
        assert!(state.get().is_none());
    }

    #[test]
    fn reload_picks_up_a_later_write() {
        let dir = ScratchDir::new("reload");
        let mut state = SchemaState::new(Some(dir.0.clone()));
        assert!(state.get().is_none());

        // Ensure a distinct mtime from "file didn't exist" (None).
        std::thread::sleep(std::time::Duration::from_millis(10));
        dir.write_schema(&empty_schema_json());
        state.reload_if_changed();
        assert!(state.get().is_some());
    }
}
