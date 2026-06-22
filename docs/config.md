# Pylon configuration reference

Pylon can be configured via a `pylon.toml` file (for the CLI and file-based workflows) or programmatically via Python dataclasses (for runtime use). Both surfaces share the same underlying dataclasses — `load_config()` parses a `pylon.toml` and returns the same `Config` object you would construct by hand.

---

# `pylon.toml`

The CLI resolves `pylon.toml` by checking the current directory and walking up the directory tree until the file is found. The first file encountered wins — no merging across directories.

## Resolution precedence

```
CLI flags  >  env vars  >  [section.branch]  >  [section]  >  defaults
```

---

## `[project]`

**Required.**

| Key | Type | Required | Description |
|---|---|---|---|
| `schema-dir` | string | yes | Path to the schema directory. Migrations live at `schema-dir/migrations`. |
| `pyql` | string (semver) | no | PyQL query language version this project targets (e.g. `"1.0.0"`). The CLI warns if the installed Pylon version speaks a different dialect. |

**Example:**

```toml
[project]
schema-dir = "dbschema"
pyql = "1.0.0"
```

---

## `[database]`

**Required.** Supports named branch configs via `[database.branch_name]`.

Either `dsn` or the discrete connection fields (`host`, `port`, `name`, `user`) must be provided. When both are present, `dsn` takes precedence.

| Key | Type | Required | Default | Description |
|---|---|---|---|---|
| `dsn` | string | — | — | Full connection DSN. Format: `pylon://user:password@host:port/name`. Takes precedence over discrete fields when present. |
| `host` | string | yes¹ | — | Database host. |
| `port` | integer | yes¹ | — | Database port. |
| `name` | string | yes¹ | — | Database name. |
| `user` | string | yes¹ | — | Database user. |
| `password` | string | no | — | Direct password value. Prefer `password_env`. |
| `password_env` | string | no | — | Name of the env var holding the password. |
| `pool_min_size` | integer | no | `2` | Minimum number of connections in the pool. Must be ≥ 1. |
| `pool_max_size` | integer | no | `10` | Maximum number of connections in the pool. Must be ≥ `pool_min_size`. |

¹ Required when `dsn` is not provided.

### Branch configs

Named branches (e.g. `[database.feature_auth]`) are sparse overrides of `[database]` — only keys that differ need to be specified. The base `[database]` block is always the fallback. The active branch is selected via CLI flag or env var; the loader itself always uses the base block.

Branch name format: `[a-z][a-z0-9_]*` — lowercase, no leading digits, underscores allowed.

**Example:**

```toml
[database]
host = "localhost"
port = 5432
name = "mydb"
user = "myuser"
password_env = "PYLON_DB_PASSWORD"
pool_min_size = 2
pool_max_size = 20

[database.feature_auth]
name = "mydb_feature_auth"

[database.staging]
host = "staging.internal"
name = "mydb_staging"
```

Or using a DSN:

```toml
[database]
dsn = "pylon://myuser:secret@localhost:5432/mydb"
```

---

## `[search]`

**Optional.** OpenSearch connection. Supports named branch configs via `[search.branch_name]`.

Named sub-tables are sparse overrides of the base `[search]` block — only differing keys need to be specified.

| Key | Type | Required | Description |
|---|---|---|---|
| `host` | string | yes | OpenSearch host. |
| `port` | integer | yes | OpenSearch port. |
| `user` | string | no | OpenSearch user. |
| `password` | string | no | Direct password value. Prefer `password_env`. |
| `password_env` | string | no | Name of the env var holding the password. |

**Example:**

```toml
[search]
host = "localhost"
port = 9200
password_env = "PYLON_SEARCH_PASSWORD"

[search.staging]
host = "opensearch.staging.internal"
```

---

## `[models]`

**Optional.** Default model provider connection for embedding generation and vector index population. Supports named peer connections via `[models.connection_name]`.

| Key | Type | Required | Description |
|---|---|---|---|
| `api_style` | enum | yes | `openai` or `anthropic` — determines the SDK and request format. |
| `api_url` | string | yes | Base URL of the model API endpoint. |
| `model` | string | yes | Model identifier (e.g. `text-embedding-3-small`). |
| `client_id` | string | no | Client ID for OAuth-style authentication (e.g. Azure Entra). |
| `secret` | string | no | Direct secret value. Prefer `secret_env`. |
| `secret_env` | string | no | Name of the env var holding the secret. |

### Named connections

Unlike `[database]` branches, named model connections (e.g. `[models.mistral_eu]`) are independent peer configurations, not overrides of `[models]`. Each is fully self-contained and referenced by name directly in schema definitions.

**Example:**

```toml
[models]
api_style = "openai"
api_url = "https://api.openai.com"
model = "text-embedding-3-small"
secret_env = "OPENAI_API_KEY"

[models.mistral_eu]
api_style = "openai"
api_url = "https://api.mistral.ai"
model = "mistral-embed"
secret_env = "MISTRAL_API_KEY"

[models.azure]
api_style = "openai"
api_url = "https://my-instance.openai.azure.com"
model = "text-embedding-ada-002"
client_id = "my-azure-client-id"
secret_env = "AZURE_OPENAI_SECRET"
```

---

## Full example

```toml
[project]
schema-dir = "dbschema"
pyql = "1.0.0"

[database]
host = "localhost"
port = 5432
name = "mydb"
user = "myuser"
password_env = "PYLON_DB_PASSWORD"
pool_max_size = 20

[database.feature_auth]
name = "mydb_feature_auth"

[database.staging]
host = "staging.internal"
name = "mydb_staging"

[search]
host = "localhost"
port = 9200
password_env = "PYLON_SEARCH_PASSWORD"

[search.staging]
host = "opensearch.staging.internal"

[models]
api_style = "openai"
api_url = "https://api.openai.com"
model = "text-embedding-3-small"
secret_env = "OPENAI_API_KEY"

[models.mistral_eu]
api_style = "openai"
api_url = "https://api.mistral.ai"
model = "mistral-embed"
secret_env = "MISTRAL_API_KEY"
```

---

# Python API

The Python client is configured programmatically via `Config` and its child dataclasses. No `pylon.toml` is required at runtime — file-based config is available via `load_config()` when convenient.

---

## `Client`

```python
from pylon import Client

client = Client(config=Config(...))
```

---

## `Config`

```python
from pylon import Config

Config(
    database: DatabaseConfig,
    project: ProjectConfig | None = None,
    search: SearchConfig | dict[str, SearchConfig] | None = None,
    models: ModelConfig | dict[str, ModelConfig] | None = None,
)
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `database` | `DatabaseConfig` | yes | Database connection. |
| `project` | `ProjectConfig` | no | Project settings. Ignored at runtime; populated by `load_config()`. |
| `search` | `SearchConfig \| dict[str, SearchConfig]` | no | Single search backend or named registry. |
| `models` | `ModelConfig \| dict[str, ModelConfig]` | no | Single model connection or named registry. |

When `search` or `models` is a `dict`, the key `'default'` is reserved for the primary connection. All other keys are named connections referenced by name in schema definitions.

---

## `DatabaseConfig`

Single active database connection. No named variants at runtime — branch switching is a CLI concern.

```python
from pylon import DatabaseConfig

DatabaseConfig(
    dsn: str | None = None,
    host: str | None = None,
    port: int | None = None,
    name: str | None = None,
    user: str | None = None,
    password: str | None = None,
    pool_min_size: int = 2,
    pool_max_size: int = 10,
)
```

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `dsn` | string | — | — | Full connection DSN. Format: `pylon://user:password@host:port/name`. Takes precedence over discrete fields when present. |
| `host` | string | yes¹ | — | Database host. |
| `port` | integer | yes¹ | — | Database port. |
| `name` | string | yes¹ | — | Database name. |
| `user` | string | yes¹ | — | Database user. |
| `password` | string | no | — | Database password. |
| `pool_min_size` | integer | no | `2` | Minimum number of connections in the pool. Must be ≥ 1. |
| `pool_max_size` | integer | no | `10` | Maximum number of connections in the pool. Must be ≥ `pool_min_size`. |

¹ Required when `dsn` is not provided.

---

## `SearchConfig`

```python
from pylon import SearchConfig

SearchConfig(
    host: str,
    port: int,
    user: str | None = None,
    password: str | None = None,
)
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `host` | string | yes | OpenSearch host. |
| `port` | integer | yes | OpenSearch port. |
| `user` | string | no | OpenSearch user. |
| `password` | string | no | OpenSearch password. |

---

## `ModelConfig`

```python
from pylon import ModelConfig

ModelConfig(
    api_style: Literal['openai', 'anthropic'],
    api_url: str,
    model: str,
    client_id: str | None = None,
    secret: str | None = None,
)
```

| Parameter | Type | Required | Description |
|---|---|---|---|
| `api_style` | `'openai' \| 'anthropic'` | yes | Determines the SDK and request format. |
| `api_url` | string | yes | Base URL of the model API endpoint. |
| `model` | string | yes | Model identifier (e.g. `text-embedding-3-small`). |
| `client_id` | string | no | Client ID for OAuth-style authentication (e.g. Azure Entra). |
| `secret` | string | no | API secret or key. |

---

## `load_config()`

Parses a `pylon.toml` file and returns a `Config` instance. Useful when a project already has a `pylon.toml` and wants to reuse it at runtime.

```python
from pylon.config import load_config

# Walk the directory tree upward from cwd
load_config()

# Explicit path
load_config('/path/to/pylon.toml')
```

**TOML → Python mapping:**

| TOML | Python |
|---|---|
| `[project]` | ignored at runtime |
| `[database]` | `DatabaseConfig` |
| `[search]` | `search['default']` in registry |
| `[search.name]` | `search['name']` in registry |
| `[models]` | `models['default']` in registry |
| `[models.name]` | `models['name']` in registry |

`load_config()` always normalizes `search` and `models` to the dict form internally, even when only a single connection is defined in the TOML.

---

## Examples

### Minimal — database only

```python
from pylon import Client, Config, DatabaseConfig

client = Client(
    config=Config(
        database=DatabaseConfig(
            dsn='pylon://myuser:password@localhost:5432/mydb',
        ),
    )
)
```

### Discrete fields, single search and model backend

```python
import os
from pylon import Client, Config, DatabaseConfig, SearchConfig, ModelConfig

client = Client(
    config=Config(
        database=DatabaseConfig(
            host='localhost',
            port=5432,
            name='mydb',
            user='myuser',
            password=os.environ['PYLON_DB_PASSWORD'],
        ),
        search=SearchConfig(
            host='localhost',
            port=9200,
            password=os.environ['PYLON_SEARCH_PASSWORD'],
        ),
        models=ModelConfig(
            api_style='openai',
            api_url='https://api.openai.com',
            model='text-embedding-3-small',
            secret=os.environ['OPENAI_API_KEY'],
        ),
    )
)
```

### Multiple search backends and model connections

```python
import os
from pylon import Client, Config, DatabaseConfig, SearchConfig, ModelConfig

client = Client(
    config=Config(
        database=DatabaseConfig(
            host='localhost',
            port=5432,
            name='mydb',
            user='myuser',
            password=os.environ['PYLON_DB_PASSWORD'],
        ),
        search={
            'default': SearchConfig(
                host='localhost',
                port=9200,
                password=os.environ['PYLON_SEARCH_PASSWORD'],
            ),
            'staging': SearchConfig(
                host='opensearch.staging.internal',
                port=9200,
                password=os.environ['PYLON_SEARCH_STAGING_PASSWORD'],
            ),
        },
        models={
            'default': ModelConfig(
                api_style='openai',
                api_url='https://api.openai.com',
                model='text-embedding-3-small',
                secret=os.environ['OPENAI_API_KEY'],
            ),
            'mistral_eu': ModelConfig(
                api_style='openai',
                api_url='https://api.mistral.ai',
                model='mistral-embed',
                secret=os.environ['MISTRAL_API_KEY'],
            ),
        },
    )
)
```

### From `pylon.toml`

```python
from pylon import Client
from pylon.config import load_config

client = Client(config=load_config())
```
