# Pylon

Async PostgreSQL mapper for the JALDIS platform. Provides a schema definition DSL, an PyQL query language, OpenSearch integration, and vector index support.

## Requirements

- Python 3.13+
- PostgreSQL

## Installation

```bash
pip install pylon
```

## Configuration

Pylon is configured programmatically via `Config` and its child dataclasses.

```python
import os
from pylon import Client, Config, DatabaseConfig, SearchConfig, ModelConfig

client = Client(
    config=Config(
        database=DatabaseConfig(
            host="localhost",
            port=5432,
            name="mydb",
            user="myuser",
            password=os.environ["PYLON_DB_PASSWORD"],
        ),
        search=SearchConfig(
            host="localhost",
            port=9200,
            password=os.environ["PYLON_SEARCH_PASSWORD"],
        ),
        models=ModelConfig(
            api_style="openai",
            api_url="https://api.openai.com",
            model="text-embedding-3-small",
            secret=os.environ["OPENAI_API_KEY"],
        ),
    )
)
```

A `pylon.toml` file can be used instead via `load_config()`:

```python
from pylon import Client
from pylon.config import load_config

client = Client(config=load_config())
```

See the [configuration reference](docs/config.md) for the full `pylon.toml` schema.

## Development

```bash
python -m pytest
```
