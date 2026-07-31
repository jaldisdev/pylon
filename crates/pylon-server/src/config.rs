//! Rust port of `pylon/config.py` — parses `pylon.toml` directly, with no
//! Python involved at all. This is genuinely new logic (confirmed this
//! session: no Rust TOML parser exists anywhere else in the workspace;
//! every other Rust caller only ever receives already-resolved config
//! values passed in from Python) — `load_config`'s real behavior is more
//! than a flat deserialize, so this mirrors its exact structure: a
//! `[database]` base block plus named `[database.<name>]` sub-tables as
//! *sparse overrides*, `[search]` sub-tables likewise, `[models]`
//! sub-tables as *independent peer configs* (not overrides), and
//! `password`/`secret`/`api_key` fields resolvable either directly or via
//! a `*_env` environment-variable name.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

type Table = toml::Table;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    /// Resolved to an absolute path relative to the directory containing
    /// `pylon.toml` — no `~` expansion (Python's own loader doesn't apply
    /// one here either, unlike `CacheConfig.path`).
    pub schema_dir: PathBuf,
    pub name: Option<String>,
    pub pyql: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    pub dsn: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub name: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub pool_min_size: u32,
    pub pool_max_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchServiceBackend {
    OpenSearch,
    Meilisearch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchConfig {
    pub host: String,
    pub port: u16,
    pub backend: SearchServiceBackend,
    pub user: Option<String>,
    pub password: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiStyle {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPurpose {
    Embedding,
    Chat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfig {
    pub api_style: ApiStyle,
    pub api_url: String,
    pub model: String,
    pub client_id: Option<String>,
    pub secret: Option<String>,
    pub purpose: ModelPurpose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebserverConfig {
    pub host: String,
    pub port: u16,
}

impl Default for WebserverConfig {
    fn default() -> Self {
        Self { host: "localhost".to_string(), port: 5656 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiConfig {
    pub enabled: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSetConfig {
    pub enabled: bool,
}

impl Default for CacheSetConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    pub enabled: bool,
    /// Always `"lmdb"` today — kept as a field (rather than a unit type)
    /// since `pylon.toml` still lets a user write `backend = "lmdb"`
    /// explicitly, matching Python's own `Literal["lmdb"]`.
    pub backend: String,
    pub max_size_mb: u64,
    pub path: PathBuf,
    pub sets: HashMap<String, CacheSetConfig>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { enabled: false, backend: "lmdb".to_string(), max_size_mb: 1024, path: PathBuf::new(), sets: HashMap::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub database: DatabaseConfig,
    /// Unlike Python's `Config.project: ProjectConfig | None` (which only
    /// allows constructing a `Config` directly without `[project]`),
    /// `load_config` itself always requires `[project]` — this is the sole
    /// real entry point ported here, so this is non-optional.
    pub project: ProjectConfig,
    /// Always the normalized registry form (Python's `search_registry`
    /// property) — empty when `[search]` is absent, `"default"`-keyed when
    /// only a bare base block was given.
    pub search: HashMap<String, SearchConfig>,
    pub models: HashMap<String, ModelConfig>,
    pub connections: HashMap<String, DatabaseConfig>,
    pub webserver: WebserverConfig,
    pub ui: UiConfig,
    pub metrics: MetricsConfig,
    pub cache: CacheConfig,
    pub toml_path: PathBuf,
}

impl Config {
    pub fn models_registry(&self) -> &HashMap<String, ModelConfig> {
        &self.models
    }

    pub fn search_registry(&self) -> &HashMap<String, SearchConfig> {
        &self.search
    }
}

const DATABASE_SCALAR_KEYS: &[&str] =
    &["dsn", "host", "port", "name", "user", "password", "password_env", "pool_min_size", "pool_max_size"];
const SEARCH_SCALAR_KEYS: &[&str] = &["host", "port", "backend", "user", "password", "password_env", "api_key", "api_key_env"];
const MODELS_SCALAR_KEYS: &[&str] = &["api_style", "api_url", "model", "client_id", "secret", "secret_env", "purpose"];

fn get_str(t: &Table, key: &str) -> Option<String> {
    t.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn get_int(t: &Table, key: &str) -> Option<i64> {
    t.get(key).and_then(|v| v.as_integer())
}

fn get_bool(t: &Table, key: &str) -> Option<bool> {
    t.get(key).and_then(|v| v.as_bool())
}

fn scalar_subset(t: &Table, keys: &[&str]) -> Table {
    t.iter().filter(|(k, _)| keys.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn merged_with(base: &Table, overrides: &Table) -> Table {
    let mut merged = base.clone();
    for (k, v) in overrides {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

/// Direct value takes precedence over the `*_env` environment-variable
/// indirection — mirrors `pylon/config.py::_resolve_secret`.
fn resolve_secret(raw: &Table, secret_key: &str, env_key: &str) -> Option<String> {
    if let Some(direct) = get_str(raw, secret_key) {
        return Some(direct);
    }
    let env_name = get_str(raw, env_key)?;
    std::env::var(env_name).ok()
}

fn build_database(raw: &Table) -> Result<DatabaseConfig> {
    let pool_min_size = get_int(raw, "pool_min_size").unwrap_or(2) as u32;
    let pool_max_size = get_int(raw, "pool_max_size").unwrap_or(10) as u32;
    let dsn = get_str(raw, "dsn");

    let (host, port, name, user, password) = if dsn.is_some() {
        (None, None, None, None, None)
    } else {
        let missing: Vec<&str> = ["host", "port", "name", "user"].into_iter().filter(|k| raw.get(*k).is_none()).collect();
        if !missing.is_empty() {
            return Err(Error::Invalid(format!(
                "DatabaseConfig: missing required fields when dsn is not provided: {}",
                missing.join(", ")
            )));
        }
        let password = resolve_secret(raw, "password", "password_env");
        (get_str(raw, "host"), get_int(raw, "port").map(|p| p as u16), get_str(raw, "name"), get_str(raw, "user"), password)
    };

    if pool_min_size < 1 {
        return Err(Error::Invalid("DatabaseConfig: pool_min_size must be >= 1.".to_string()));
    }
    if pool_max_size < pool_min_size {
        return Err(Error::Invalid("DatabaseConfig: pool_max_size must be >= pool_min_size.".to_string()));
    }

    Ok(DatabaseConfig { dsn, host, port, name, user, password, pool_min_size, pool_max_size })
}

fn build_search(raw: &Table) -> Result<SearchConfig> {
    let password = resolve_secret(raw, "password", "password_env");
    let api_key = resolve_secret(raw, "api_key", "api_key_env");
    let backend = match get_str(raw, "backend").as_deref().unwrap_or("opensearch") {
        "opensearch" => SearchServiceBackend::OpenSearch,
        "meilisearch" => SearchServiceBackend::Meilisearch,
        other => {
            return Err(Error::Invalid(format!("SearchConfig: backend must be 'opensearch' or 'meilisearch', got {other:?}")))
        }
    };
    let host = get_str(raw, "host").ok_or(Error::MissingField { section: "search", field: "host" })?;
    let port = get_int(raw, "port").ok_or(Error::MissingField { section: "search", field: "port" })? as u16;
    Ok(SearchConfig { host, port, backend, user: get_str(raw, "user"), password, api_key })
}

fn build_model(raw: &Table) -> Result<ModelConfig> {
    let api_style = match get_str(raw, "api_style").as_deref() {
        Some("openai") => ApiStyle::OpenAi,
        Some("anthropic") => ApiStyle::Anthropic,
        other => {
            return Err(Error::Invalid(format!("ModelConfig: api_style must be 'openai' or 'anthropic', got {other:?}")))
        }
    };
    let purpose = match get_str(raw, "purpose").as_deref().unwrap_or("embedding") {
        "embedding" => ModelPurpose::Embedding,
        "chat" => ModelPurpose::Chat,
        other => return Err(Error::Invalid(format!("ModelConfig: purpose must be 'embedding' or 'chat', got {other:?}"))),
    };
    let secret = resolve_secret(raw, "secret", "secret_env");
    let api_url = get_str(raw, "api_url").ok_or(Error::MissingField { section: "models", field: "api_url" })?;
    let model = get_str(raw, "model").ok_or(Error::MissingField { section: "models", field: "model" })?;
    Ok(ModelConfig { api_style, api_url, model, client_id: get_str(raw, "client_id"), secret, purpose })
}

fn build_cache_set(raw: &Table) -> CacheSetConfig {
    CacheSetConfig { enabled: get_bool(raw, "enabled").unwrap_or(true) }
}

/// `~` expansion for `[cache].path` only — matches `Path.expanduser()`,
/// which Python's loader applies there but not to `schema-dir`.
fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if input == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(input)
}

/// Lexically collapses `.`/`..` components without touching the
/// filesystem (unlike `std::fs::canonicalize`, which requires the path to
/// already exist) — a deliberately simplified stand-in for Python's
/// `Path.resolve()`, which doesn't require existence either but also
/// resolves symlinks; good enough for a config path that may not exist
/// yet (e.g. a cache directory created lazily on first use).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn resolve_cache_path(raw_path: Option<&str>, toml_path: &Path) -> PathBuf {
    let expanded = match raw_path {
        Some(p) => expand_tilde(p),
        None => PathBuf::from(".pylon/cache"),
    };
    if expanded.is_absolute() {
        return expanded;
    }
    let base = toml_path.parent().unwrap_or_else(|| Path::new("."));
    lexical_normalize(&base.join(expanded))
}

fn find_toml(start: &Path) -> Result<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join("pylon.toml");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(Error::TomlNotFound(start.to_path_buf()))
}

/// Parses `pylon.toml`, discovered by walking up from `start_dir` (or the
/// current working directory when `None`) — mirrors
/// `pylon/config.py::load_config`.
pub fn load_config(start_dir: Option<&Path>) -> Result<Config> {
    let start = match start_dir {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().map_err(|e| Error::Io { path: PathBuf::from("."), source: e })?,
    };
    let toml_path = find_toml(&start)?;

    let text = std::fs::read_to_string(&toml_path).map_err(|e| Error::Io { path: toml_path.clone(), source: e })?;
    let raw: Table = text.parse().map_err(|e| Error::TomlParse { path: toml_path.clone(), source: e })?;

    // ── [database] ──────────────────────────────────────────────────────
    let raw_db = raw.get("database").and_then(|v| v.as_table()).ok_or(Error::MissingSection("database"))?;
    let base_db_raw = scalar_subset(raw_db, DATABASE_SCALAR_KEYS);
    let database = build_database(&base_db_raw)?;

    let mut connections = HashMap::new();
    connections.insert("default".to_string(), database.clone());
    for (key, value) in raw_db {
        if DATABASE_SCALAR_KEYS.contains(&key.as_str()) {
            continue;
        }
        if let Some(sub) = value.as_table() {
            connections.insert(key.clone(), build_database(&merged_with(&base_db_raw, sub))?);
        }
    }

    // ── [search] — named sub-tables are sparse overrides ────────────────
    let mut search = HashMap::new();
    if let Some(raw_search) = raw.get("search").and_then(|v| v.as_table()) {
        let base_search_raw = scalar_subset(raw_search, SEARCH_SCALAR_KEYS);
        if !base_search_raw.is_empty() {
            search.insert("default".to_string(), build_search(&base_search_raw)?);
        }
        for (key, value) in raw_search {
            if SEARCH_SCALAR_KEYS.contains(&key.as_str()) {
                continue;
            }
            if let Some(sub) = value.as_table() {
                search.insert(key.clone(), build_search(&merged_with(&base_search_raw, sub))?);
            }
        }
    }

    // ── [models] — named sub-tables are independent peer configs ───────
    let mut models = HashMap::new();
    if let Some(raw_models) = raw.get("models").and_then(|v| v.as_table()) {
        let base_models_raw = scalar_subset(raw_models, MODELS_SCALAR_KEYS);
        if !base_models_raw.is_empty() {
            models.insert("default".to_string(), build_model(&base_models_raw)?);
        }
        for (key, value) in raw_models {
            if MODELS_SCALAR_KEYS.contains(&key.as_str()) {
                continue;
            }
            if let Some(sub) = value.as_table() {
                models.insert(key.clone(), build_model(sub)?);
            }
        }
    }

    // ── [project] ────────────────────────────────────────────────────────
    let raw_project = raw.get("project").and_then(|v| v.as_table()).ok_or(Error::MissingSection("project"))?;
    let schema_dir_raw =
        get_str(raw_project, "schema-dir").ok_or(Error::MissingField { section: "project", field: "schema-dir" })?;
    let toml_dir = toml_path.parent().unwrap_or_else(|| Path::new("."));
    let project = ProjectConfig {
        schema_dir: lexical_normalize(&toml_dir.join(schema_dir_raw)),
        name: get_str(raw_project, "name"),
        pyql: get_str(raw_project, "pyql"),
    };

    // ── [webserver] / [ui] / [metrics] ──────────────────────────────────
    let webserver = raw.get("webserver").and_then(|v| v.as_table()).map_or_else(WebserverConfig::default, |t| {
        WebserverConfig {
            host: get_str(t, "host").unwrap_or_else(|| WebserverConfig::default().host),
            port: get_int(t, "port").map(|p| p as u16).unwrap_or_else(|| WebserverConfig::default().port),
        }
    });
    let ui = raw
        .get("ui")
        .and_then(|v| v.as_table())
        .map_or(UiConfig::default(), |t| UiConfig { enabled: get_bool(t, "enabled").unwrap_or(true) });
    let metrics = raw
        .get("metrics")
        .and_then(|v| v.as_table())
        .map_or(MetricsConfig::default(), |t| MetricsConfig { enabled: get_bool(t, "enabled").unwrap_or(false) });

    // ── [cache] ──────────────────────────────────────────────────────────
    let mut cache = CacheConfig::default();
    let mut raw_cache_path = None;
    if let Some(rc) = raw.get("cache").and_then(|v| v.as_table()) {
        if let Some(v) = get_bool(rc, "enabled") {
            cache.enabled = v;
        }
        if let Some(v) = get_str(rc, "backend") {
            if v != "lmdb" {
                return Err(Error::Invalid(format!("CacheConfig: backend must be 'lmdb', got {v:?}")));
            }
            cache.backend = v;
        }
        if let Some(v) = get_int(rc, "max_size_mb") {
            cache.max_size_mb = v as u64;
        }
        raw_cache_path = get_str(rc, "path");
        if let Some(sets_table) = rc.get("sets").and_then(|v| v.as_table()) {
            cache.sets = sets_table.iter().filter_map(|(k, v)| v.as_table().map(|t| (k.clone(), build_cache_set(t)))).collect();
        }
    }
    cache.path = resolve_cache_path(raw_cache_path.as_deref(), &toml_path);

    Ok(Config { database, project, search, models, connections, webserver, ui, metrics, cache, toml_path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("pylon-server-config-test-{tag}-{nanos}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write_toml(&self, contents: &str) {
            std::fs::write(self.0.join("pylon.toml"), contents).unwrap();
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_toml_is_an_error() {
        let dir = ScratchDir::new("missing");
        std::fs::remove_file(dir.0.join("pylon.toml")).ok();
        assert!(matches!(load_config(Some(&dir.0)), Err(Error::TomlNotFound(_))));
    }

    #[test]
    fn minimal_config_with_discrete_database_fields() {
        let dir = ScratchDir::new("minimal");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "app"
            user = "postgres"
            "#,
        );
        let config = load_config(Some(&dir.0)).unwrap();
        assert_eq!(config.database.host.as_deref(), Some("localhost"));
        assert_eq!(config.database.port, Some(5432));
        assert_eq!(config.database.pool_min_size, 2);
        assert_eq!(config.database.pool_max_size, 10);
        assert_eq!(config.project.schema_dir, dir.0.join("dbschema"));
        assert_eq!(config.connections.len(), 1);
        assert!(config.connections.contains_key("default"));
    }

    #[test]
    fn dsn_form_skips_discrete_field_requirement() {
        let dir = ScratchDir::new("dsn");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            dsn = "postgresql://u:p@host/db"
            "#,
        );
        let config = load_config(Some(&dir.0)).unwrap();
        assert_eq!(config.database.dsn.as_deref(), Some("postgresql://u:p@host/db"));
        assert_eq!(config.database.host, None);
    }

    #[test]
    fn missing_discrete_fields_without_dsn_is_an_error() {
        let dir = ScratchDir::new("missing-fields");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            "#,
        );
        assert!(matches!(load_config(Some(&dir.0)), Err(Error::Invalid(_))));
    }

    #[test]
    fn named_database_sub_table_is_a_sparse_override() {
        let dir = ScratchDir::new("db-override");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "app"
            user = "postgres"

            [database.staging]
            name = "app_staging"
            "#,
        );
        let config = load_config(Some(&dir.0)).unwrap();
        assert_eq!(config.connections.len(), 2);
        let staging = &config.connections["staging"];
        assert_eq!(staging.name.as_deref(), Some("app_staging"));
        // Inherited from the base block, not re-specified.
        assert_eq!(staging.host.as_deref(), Some("localhost"));
        assert_eq!(staging.user.as_deref(), Some("postgres"));
    }

    #[test]
    fn models_named_sub_tables_are_independent_not_merged() {
        let dir = ScratchDir::new("models-peer");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            dsn = "postgresql://u:p@host/db"

            [models]
            api_style = "openai"
            api_url = "https://api.openai.com/v1"
            model = "text-embedding-3-small"

            [models.chat]
            api_style = "anthropic"
            api_url = "https://api.anthropic.com/v1"
            model = "claude-3"
            purpose = "chat"
            "#,
        );
        let config = load_config(Some(&dir.0)).unwrap();
        assert_eq!(config.models.len(), 2);
        let chat = &config.models["chat"];
        assert_eq!(chat.api_style, ApiStyle::Anthropic);
        // Not inherited from the base [models] block — a peer, not an override.
        assert_eq!(chat.model, "claude-3");
    }

    #[test]
    fn password_env_indirection_resolves_from_environment() {
        let dir = ScratchDir::new("secret-env");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "app"
            user = "postgres"
            password_env = "PYLON_SERVER_TEST_DB_PASSWORD"
            "#,
        );
        std::env::set_var("PYLON_SERVER_TEST_DB_PASSWORD", "s3cret");
        let config = load_config(Some(&dir.0)).unwrap();
        std::env::remove_var("PYLON_SERVER_TEST_DB_PASSWORD");
        assert_eq!(config.database.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn direct_password_takes_precedence_over_env_indirection() {
        let dir = ScratchDir::new("secret-direct");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "app"
            user = "postgres"
            password = "direct"
            password_env = "PYLON_SERVER_TEST_DB_PASSWORD_UNUSED"
            "#,
        );
        let config = load_config(Some(&dir.0)).unwrap();
        assert_eq!(config.database.password.as_deref(), Some("direct"));
    }

    #[test]
    fn cache_path_defaults_relative_to_toml_directory() {
        let dir = ScratchDir::new("cache-default");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            dsn = "postgresql://u:p@host/db"
            "#,
        );
        let config = load_config(Some(&dir.0)).unwrap();
        assert_eq!(config.cache.path, dir.0.join(".pylon/cache"));
        assert!(!config.cache.enabled);
    }

    #[test]
    fn webserver_ui_metrics_defaults() {
        let dir = ScratchDir::new("defaults");
        dir.write_toml(
            r#"
            [project]
            schema-dir = "dbschema"

            [database]
            dsn = "postgresql://u:p@host/db"
            "#,
        );
        let config = load_config(Some(&dir.0)).unwrap();
        assert_eq!(config.webserver, WebserverConfig::default());
        assert_eq!(config.ui.enabled, true);
        assert_eq!(config.metrics.enabled, false);
    }
}
