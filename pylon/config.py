from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

import tomllib


# ---------------------------------------------------------------------------
# ProjectConfig
# ---------------------------------------------------------------------------


@dataclass(slots=True, frozen=True)
class ProjectConfig:
    """Project-level settings from the ``[project]`` TOML section.

    *schema_dir* is resolved to an absolute path relative to the directory
    that contains ``pylon.toml``.
    """

    schema_dir: Path
    name: str | None = None
    pyql: str | None = None


# ---------------------------------------------------------------------------
# DatabaseConfig
# ---------------------------------------------------------------------------


@dataclass(slots=True, frozen=True)
class DatabaseConfig:
    """Single active database connection.

    Either *dsn* or the discrete fields (*host*, *port*, *name*, *user*,
    *password*) must be supplied.  When both are present *dsn* takes
    precedence.

    DSN format: ``pylon://user:password@host:port/name``
    """

    dsn: str | None = None
    host: str | None = None
    port: int | None = None
    name: str | None = None
    user: str | None = None
    password: str | None = None
    pool_min_size: int = 2
    pool_max_size: int = 10

    def __post_init__(self) -> None:
        if self.dsn is None:
            missing = [
                k for k in ("host", "port", "name", "user") if getattr(self, k) is None
            ]
            if missing:
                raise ValueError(
                    f"DatabaseConfig: missing required fields when dsn is not "
                    f"provided: {', '.join(missing)}"
                )
        if self.pool_min_size < 1:
            raise ValueError("DatabaseConfig: pool_min_size must be >= 1.")
        if self.pool_max_size < self.pool_min_size:
            raise ValueError(
                "DatabaseConfig: pool_max_size must be >= pool_min_size."
            )


# ---------------------------------------------------------------------------
# SearchConfig
# ---------------------------------------------------------------------------


@dataclass(slots=True, frozen=True)
class SearchConfig:
    """Search backend connection (OpenSearch or Meilisearch)."""

    host: str
    port: int
    backend: Literal["opensearch", "meilisearch"] = "opensearch"
    # OpenSearch: HTTP basic auth
    user: str | None = None
    password: str | None = None
    # Meilisearch: bearer API key
    api_key: str | None = None


# ---------------------------------------------------------------------------
# ModelConfig
# ---------------------------------------------------------------------------


@dataclass(slots=True, frozen=True)
class ModelConfig:
    """Model provider connection for embedding generation and vector index population."""

    api_style: Literal["openai", "anthropic"]
    api_url: str
    model: str
    client_id: str | None = None
    secret: str | None = None


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

# Convenience alias used internally and exposed for type annotations.
type SearchRegistry = dict[str, SearchConfig]
type ModelRegistry = dict[str, ModelConfig]


@dataclass(slots=True, frozen=True)
class Config:
    """Top-level Pylon configuration.

    *search* and *models* each accept either a single instance (implicitly
    bound as ``'default'``) or a named registry dict.  When a dict is
    supplied the key ``'default'`` is reserved for the primary connection.

    *connections* maps every named ``[database.<name>]`` sub-table (plus
    ``'default'`` for the base ``[database]`` block) to its resolved
    :class:`DatabaseConfig`.  Pass ``-d <name>`` on the CLI to select one.
    """

    database: DatabaseConfig
    project: ProjectConfig | None = None
    search: SearchConfig | SearchRegistry | None = None
    models: ModelConfig | ModelRegistry | None = None
    connections: dict[str, DatabaseConfig] = field(default_factory=dict)

    # ------------------------------------------------------------------
    # Normalised accessors
    # ------------------------------------------------------------------

    @property
    def search_registry(self) -> SearchRegistry:
        """Always returns the search config as a named registry."""
        match self.search:
            case None:
                return {}
            case SearchConfig() as s:
                return {"default": s}
            case dict() as d:
                return d

    @property
    def models_registry(self) -> ModelRegistry:
        """Always returns the model config as a named registry."""
        match self.models:
            case None:
                return {}
            case ModelConfig() as m:
                return {"default": m}
            case dict() as d:
                return d


# ---------------------------------------------------------------------------
# TOML loader
# ---------------------------------------------------------------------------

_FILENAME = "pylon.toml"


def _find_toml(start: Path) -> Path:
    """Walk up the directory tree from *start* until ``pylon.toml`` is found."""
    for directory in (start, *start.parents):
        candidate = directory / _FILENAME
        if candidate.is_file():
            return candidate
    raise FileNotFoundError(
        f"Could not locate {_FILENAME!r} in {start} or any parent directory."
    )


def _resolve_secret(
    raw: dict[str, object], secret_key: str, env_key: str
) -> str | None:
    """Return the direct value or the env-var expansion, preferring direct."""
    if direct := raw.get(secret_key):
        return str(direct)
    if env_name := raw.get(env_key):
        return os.environ.get(str(env_name))
    return None


def _build_database(raw: dict[str, object]) -> DatabaseConfig:
    pool_min = int(raw.get("pool_min_size", 2))  # type: ignore[arg-type]
    pool_max = int(raw.get("pool_max_size", 10))  # type: ignore[arg-type]
    if dsn := raw.get("dsn"):
        return DatabaseConfig(dsn=str(dsn), pool_min_size=pool_min, pool_max_size=pool_max)
    password = _resolve_secret(raw, "password", "password_env")
    return DatabaseConfig(
        host=str(raw["host"]),
        port=int(raw["port"]),  # type: ignore[arg-type]
        name=str(raw["name"]),
        user=str(raw["user"]),
        password=password,
        pool_min_size=pool_min,
        pool_max_size=pool_max,
    )


def _build_search(raw: dict[str, object]) -> SearchConfig:
    password = _resolve_secret(raw, "password", "password_env")
    api_key = _resolve_secret(raw, "api_key", "api_key_env")
    user_val = raw.get("user")
    backend_val = raw.get("backend", "opensearch")
    if backend_val not in ("opensearch", "meilisearch"):
        raise ValueError(f"SearchConfig: backend must be 'opensearch' or 'meilisearch', got {backend_val!r}")
    return SearchConfig(
        host=str(raw["host"]),
        port=int(raw["port"]),  # type: ignore[arg-type]
        backend=backend_val,  # type: ignore[arg-type]
        user=str(user_val) if user_val is not None else None,
        password=password,
        api_key=api_key,
    )


def _build_model(raw: dict[str, object]) -> ModelConfig:
    api_style = raw["api_style"]
    if api_style not in ("openai", "anthropic"):
        raise ValueError(
            f"ModelConfig: api_style must be 'openai' or 'anthropic', got {api_style!r}"
        )
    secret = _resolve_secret(raw, "secret", "secret_env")
    client_id_val = raw.get("client_id")
    return ModelConfig(
        api_style=api_style,  # type: ignore[arg-type]
        api_url=str(raw["api_url"]),
        model=str(raw["model"]),
        client_id=str(client_id_val) if client_id_val is not None else None,
        secret=secret,
    )


# Keys that are reserved TOML table names and must not be treated as branch/
# connection names when iterating a section for sub-tables.
_RESERVED_TOP_LEVEL = frozenset({"project", "database", "search", "models"})

# Known scalar keys in each section — sub-tables within a section are named
# connections/branches.
_DATABASE_SCALAR_KEYS = frozenset(
    {"dsn", "host", "port", "name", "user", "password", "password_env", "pool_min_size", "pool_max_size"}
)
_SEARCH_SCALAR_KEYS = frozenset({"host", "port", "user", "password", "password_env"})
_MODELS_SCALAR_KEYS = frozenset(
    {"api_style", "api_url", "model", "client_id", "secret", "secret_env"}
)


def load_config(path: str | Path | None = None) -> Config:
    """Parse a ``pylon.toml`` file and return a :class:`Config` instance.

    Args:
        path: Explicit path to ``pylon.toml``.  When omitted the directory
              tree is walked upward from the current working directory.

    Returns:
        A fully-constructed :class:`Config`.

    Raises:
        FileNotFoundError: When no ``pylon.toml`` can be located.
        KeyError / ValueError: When required keys are absent or invalid.

    Notes:
        ``[project]`` is required and must include ``schema-dir``.

        ``[search]`` and ``[models]`` are always normalised to the registry
        (dict) form internally, even when only a single connection is defined.

        For ``[database]``, named sub-tables (``[database.branch_name]``) are
        *sparse overrides* — only the differing keys need to be specified.
        The base ``[database]`` block is the fallback.  The active branch is
        selected via CLI flag or env var at the CLI layer, not here; this
        loader always uses the base ``[database]`` block.

        For ``[models]``, named sub-tables are independent peer configs, not
        overrides.
    """
    toml_path = _find_toml(Path(path).resolve() if path else Path.cwd())

    with toml_path.open("rb") as fh:
        raw: dict[str, object] = tomllib.load(fh)

    # ------------------------------------------------------------------
    # [database]
    # ------------------------------------------------------------------
    raw_db = raw.get("database")
    if not isinstance(raw_db, dict):
        raise KeyError("pylon.toml: required section [database] is missing or invalid.")

    # Extract only scalar keys for the base config; sub-tables are named connections.
    base_db_raw = {k: v for k, v in raw_db.items() if k in _DATABASE_SCALAR_KEYS}
    database = _build_database(base_db_raw)

    # Build the named-connections registry.  "default" is always the base block;
    # each sub-table is a sparse override (inherits unset keys from the base).
    connections: dict[str, DatabaseConfig] = {"default": database}
    for key, value in raw_db.items():
        if key in _DATABASE_SCALAR_KEYS or not isinstance(value, dict):
            continue
        merged = {**base_db_raw, **value}
        connections[key] = _build_database(merged)

    # ------------------------------------------------------------------
    # [search]
    # ------------------------------------------------------------------
    search: SearchConfig | SearchRegistry | None = None
    raw_search = raw.get("search")

    if isinstance(raw_search, dict):
        base_search_raw = {
            k: v for k, v in raw_search.items() if k in _SEARCH_SCALAR_KEYS
        }
        registry: SearchRegistry = {}

        if base_search_raw:
            registry["default"] = _build_search(base_search_raw)

        for key, value in raw_search.items():
            if key in _SEARCH_SCALAR_KEYS:
                continue
            if isinstance(value, dict):
                # Named search connections are sparse overrides of the base —
                # same pattern as [database.branch_name].
                merged = {**base_search_raw, **value}
                registry[key] = _build_search(merged)

        search = registry if registry else None

    # ------------------------------------------------------------------
    # [models]
    # ------------------------------------------------------------------
    models: ModelConfig | ModelRegistry | None = None
    raw_models = raw.get("models")

    if isinstance(raw_models, dict):
        base_models_raw = {
            k: v for k, v in raw_models.items() if k in _MODELS_SCALAR_KEYS
        }
        model_registry: ModelRegistry = {}

        if base_models_raw:
            model_registry["default"] = _build_model(base_models_raw)

        for key, value in raw_models.items():
            if key in _MODELS_SCALAR_KEYS:
                continue
            if isinstance(value, dict):
                model_registry[key] = _build_model(value)

        models = model_registry if model_registry else None

    # ------------------------------------------------------------------
    # [project]
    # ------------------------------------------------------------------
    raw_project = raw.get("project")
    if not isinstance(raw_project, dict):
        raise KeyError("pylon.toml: required section [project] is missing or invalid.")

    schema_dir_raw = raw_project.get("schema-dir")
    if not schema_dir_raw:
        raise KeyError("pylon.toml: [project] requires 'schema-dir'.")

    schema_dir = (toml_path.parent / str(schema_dir_raw)).resolve()
    name_raw = raw_project.get("name")
    pyql_raw = raw_project.get("pyql")
    project = ProjectConfig(
        schema_dir=schema_dir,
        name=str(name_raw) if name_raw is not None else None,
        pyql=str(pyql_raw) if pyql_raw is not None else None,
    )

    return Config(database=database, project=project, search=search, models=models, connections=connections)


__all__ = [
    "Config",
    "ProjectConfig",
    "DatabaseConfig",
    "SearchConfig",
    "ModelConfig",
    "load_config",
]
