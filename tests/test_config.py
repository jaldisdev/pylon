#
# This source file is part of the Pylon open source project.
#
# Copyright (c) 2026 Jaldis B.V.
#
# Licensed under the MIT OR Apache-2.0 license (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://opensource.org/licenses/MIT
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

from __future__ import annotations

import dataclasses
import sys
import textwrap
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent))

from pylon.config import (
    CacheConfig,
    CacheSetConfig,
    Config,
    DatabaseConfig,
    MetricsConfig,
    ModelConfig,
    ProjectConfig,
    SearchConfig,
    load_config,
)

# ---------------------------------------------------------------------------
# DatabaseConfig
# ---------------------------------------------------------------------------


class TestDatabaseConfig:
    def test_dsn_only(self):
        db = DatabaseConfig(dsn='pylon://u:p@localhost:5432/mydb')
        assert db.dsn == 'pylon://u:p@localhost:5432/mydb'

    def test_discrete_fields(self):
        db = DatabaseConfig(host='localhost', port=5432, name='mydb', user='u', password='p')
        assert db.host == 'localhost'
        assert db.port == 5432
        assert db.name == 'mydb'
        assert db.user == 'u'
        assert db.password == 'p'

    def test_missing_required_fields_raises(self):
        with pytest.raises(ValueError, match='host'):
            DatabaseConfig(port=5432, name='mydb', user='u')

    def test_dsn_skips_validation(self):
        # All discrete fields missing but dsn present — should not raise.
        db = DatabaseConfig(dsn='pylon://u:p@host:5432/db')
        assert db.dsn is not None

    def test_frozen(self):
        db = DatabaseConfig(dsn='pylon://u:p@h:5432/db')
        with pytest.raises(dataclasses.FrozenInstanceError):
            db.dsn = 'other'  # type: ignore[misc]


# ---------------------------------------------------------------------------
# SearchConfig
# ---------------------------------------------------------------------------


class TestSearchConfig:
    def test_required_fields(self):
        s = SearchConfig(host='localhost', port=9200)
        assert s.host == 'localhost'
        assert s.port == 9200
        assert s.user is None
        assert s.password is None

    def test_with_auth(self):
        s = SearchConfig(host='h', port=9200, user='admin', password='secret')
        assert s.user == 'admin'
        assert s.password == 'secret'


# ---------------------------------------------------------------------------
# ModelConfig
# ---------------------------------------------------------------------------


class TestModelConfig:
    def test_openai_style(self):
        m = ModelConfig(
            api_style='openai',
            api_url='https://api.openai.com',
            model='text-embedding-3-small',
            secret='sk-xxx',
        )
        assert m.api_style == 'openai'

    def test_anthropic_style(self):
        m = ModelConfig(
            api_style='anthropic',
            api_url='https://api.anthropic.com',
            model='claude-4-haiku',
            secret='sk-ant-xxx',
        )
        assert m.api_style == 'anthropic'

    def test_oauth_fields(self):
        m = ModelConfig(
            api_style='openai',
            api_url='https://my-instance.openai.azure.com',
            model='text-embedding-ada-002',
            client_id='my-azure-client-id',
            secret='azure-secret',
        )
        assert m.client_id == 'my-azure-client-id'


# ---------------------------------------------------------------------------
# Config normalisation
# ---------------------------------------------------------------------------


class TestConfigNormalisation:
    def _db(self) -> DatabaseConfig:
        return DatabaseConfig(dsn='pylon://u:p@h:5432/db')

    def _project(self) -> ProjectConfig:
        return ProjectConfig(schema_dir=Path('/tmp/schema'))

    def test_search_none(self):
        c = Config(database=self._db())
        assert c.search_registry == {}

    def test_search_single_instance_normalised(self):
        s = SearchConfig(host='h', port=9200)
        c = Config(database=self._db(), search=s)
        assert c.search_registry == {'default': s}

    def test_search_dict_passthrough(self):
        s1 = SearchConfig(host='h1', port=9200)
        s2 = SearchConfig(host='h2', port=9200)
        c = Config(database=self._db(), search={'default': s1, 'staging': s2})
        assert c.search_registry['staging'] is s2

    def test_models_single_instance_normalised(self):
        m = ModelConfig(api_style='openai', api_url='https://api.openai.com', model='m')
        c = Config(database=self._db(), models=m)
        assert c.models_registry == {'default': m}

    def test_models_dict_passthrough(self):
        m1 = ModelConfig(api_style='openai', api_url='https://api.openai.com', model='m1')
        m2 = ModelConfig(api_style='openai', api_url='https://api.mistral.ai', model='m2')
        c = Config(database=self._db(), models={'default': m1, 'mistral_eu': m2})
        assert c.models_registry['mistral_eu'] is m2


# ---------------------------------------------------------------------------
# load_config — TOML parsing
# ---------------------------------------------------------------------------


@pytest.fixture()
def toml_dir(tmp_path: Path):
    """Returns a factory that writes a pylon.toml to a temp dir."""

    def _write(content: str) -> Path:
        p = tmp_path / 'pylon.toml'
        p.write_text(textwrap.dedent(content))
        return tmp_path

    return _write


class TestLoadConfig:
    def test_minimal_database_only(self, toml_dir):
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
            password = "secret"
        """)
        cfg = load_config(d / 'pylon.toml')
        assert cfg.database.host == 'localhost'
        assert cfg.database.password == 'secret'
        assert cfg.search is None
        assert cfg.models is None

    def test_password_env(self, toml_dir, monkeypatch):
        monkeypatch.setenv('MY_PW', 'env_password')
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
            password_env = "MY_PW"
        """)
        cfg = load_config(d / 'pylon.toml')
        assert cfg.database.password == 'env_password'

    def test_search_section(self, toml_dir):
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"

            [search]
            host = "localhost"
            port = 9200
            password = "opensearch_pw"
        """)
        cfg = load_config(d / 'pylon.toml')
        assert isinstance(cfg.search, dict)
        assert cfg.search_registry['default'].host == 'localhost'
        assert cfg.search_registry['default'].password == 'opensearch_pw'

    def test_search_named_branch(self, toml_dir):
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"

            [search]
            host = "localhost"
            port = 9200

            [search.staging]
            host = "opensearch.staging.internal"
        """)
        cfg = load_config(d / 'pylon.toml')
        # Named search connections are sparse overrides — port is inherited from base.
        assert cfg.search_registry['staging'].host == 'opensearch.staging.internal'
        assert cfg.search_registry['staging'].port == 9200

    def test_models_section(self, toml_dir, monkeypatch):
        monkeypatch.setenv('OPENAI_KEY', 'sk-xxx')
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"

            [models]
            api_style = "openai"
            api_url = "https://api.openai.com"
            model = "text-embedding-3-small"
            secret_env = "OPENAI_KEY"
        """)
        cfg = load_config(d / 'pylon.toml')
        assert cfg.models_registry['default'].secret == 'sk-xxx'
        assert cfg.models_registry['default'].model == 'text-embedding-3-small'

    def test_models_named_connections(self, toml_dir, monkeypatch):
        monkeypatch.setenv('OPENAI_KEY', 'sk-openai')
        monkeypatch.setenv('MISTRAL_KEY', 'sk-mistral')
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"

            [models]
            api_style = "openai"
            api_url = "https://api.openai.com"
            model = "text-embedding-3-small"
            secret_env = "OPENAI_KEY"

            [models.mistral_eu]
            api_style = "openai"
            api_url = "https://api.mistral.ai"
            model = "mistral-embed"
            secret_env = "MISTRAL_KEY"
        """)
        cfg = load_config(d / 'pylon.toml')
        assert cfg.models_registry['default'].secret == 'sk-openai'
        assert cfg.models_registry['mistral_eu'].secret == 'sk-mistral'
        assert cfg.models_registry['mistral_eu'].api_url == 'https://api.mistral.ai'

    def test_database_branch_tables_ignored_in_base(self, toml_dir):
        """Branch sub-tables in [database] must not pollute the base config."""
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"

            [database.feature_auth]
            name = "mydb_feature_auth"

            [database.staging]
            host = "staging.internal"
            name = "mydb_staging"
        """)
        cfg = load_config(d / 'pylon.toml')
        # Base should still resolve normally.
        assert cfg.database.name == 'mydb'
        assert cfg.database.host == 'localhost'

    def test_missing_database_raises(self, toml_dir):
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"
        """)
        with pytest.raises(KeyError, match='database'):
            load_config(d / 'pylon.toml')

    def test_file_not_found_raises(self, tmp_path):
        with pytest.raises(FileNotFoundError):
            load_config(tmp_path / 'nonexistent' / 'pylon.toml')

    def test_project_section_parsed(self, toml_dir, tmp_path):
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"
            pyql = "1.0.0"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
        """)
        cfg = load_config(d / 'pylon.toml')
        assert cfg.database.name == 'mydb'
        assert cfg.project is not None
        assert cfg.project.schema_dir == (d / 'dbschema').resolve()
        assert cfg.project.pyql == '1.0.0'

    def test_project_schema_dir_without_pyql(self, toml_dir):
        d = toml_dir("""
            [project]
            schema-dir = "schema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
        """)
        cfg = load_config(d / 'pylon.toml')
        assert cfg.project is not None
        assert cfg.project.schema_dir.name == 'schema'
        assert cfg.project.pyql is None

    def test_missing_project_raises(self, toml_dir):
        d = toml_dir("""
            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
        """)
        with pytest.raises(KeyError, match='project'):
            load_config(d / 'pylon.toml')

    def test_missing_schema_dir_raises(self, toml_dir):
        d = toml_dir("""
            [project]
            pyql = "1.0.0"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
        """)
        with pytest.raises(KeyError, match='schema-dir'):
            load_config(d / 'pylon.toml')

    def test_full_example(self, toml_dir, monkeypatch):
        monkeypatch.setenv('PYLON_DB_PASSWORD', 'db_pw')
        monkeypatch.setenv('PYLON_SEARCH_PASSWORD', 'search_pw')
        monkeypatch.setenv('OPENAI_API_KEY', 'sk-oai')
        monkeypatch.setenv('MISTRAL_API_KEY', 'sk-mis')
        d = toml_dir("""
            [project]
            schema-dir = "dbschema"
            pyql = "1.0.0"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
            password_env = "PYLON_DB_PASSWORD"

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
        """)
        cfg = load_config(d / 'pylon.toml')
        assert cfg.database.password == 'db_pw'
        assert cfg.search_registry['default'].password == 'search_pw'
        assert cfg.search_registry['staging'].host == 'opensearch.staging.internal'
        assert cfg.models_registry['default'].secret == 'sk-oai'
        assert cfg.models_registry['mistral_eu'].secret == 'sk-mis'


# ---------------------------------------------------------------------------
# CacheConfig
# ---------------------------------------------------------------------------


class TestCacheConfig:
    def test_defaults(self):
        c = CacheConfig()
        assert c.enabled is False
        assert c.backend == 'lmdb'
        assert c.max_size_mb == 1024
        assert c.sets == {}

    def test_set_override_defaults_enabled(self):
        assert CacheSetConfig().enabled is True


class TestMetricsConfig:
    def test_defaults_disabled(self):
        assert MetricsConfig().enabled is False


class TestLoadConfigMetrics:
    def _base(self) -> str:
        return """
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
        """

    def test_metrics_absent_defaults_disabled(self, toml_dir):
        d = toml_dir(self._base())
        cfg = load_config(d / 'pylon.toml')
        assert cfg.metrics.enabled is False

    def test_metrics_section_enables(self, toml_dir):
        d = toml_dir(
            self._base()
            + """
            [metrics]
            enabled = true
        """
        )
        cfg = load_config(d / 'pylon.toml')
        assert cfg.metrics.enabled is True


class TestLoadConfigCache:
    def _base(self) -> str:
        return """
            [project]
            schema-dir = "dbschema"

            [database]
            host = "localhost"
            port = 5432
            name = "mydb"
            user = "myuser"
        """

    def test_cache_absent_defaults_disabled(self, toml_dir):
        d = toml_dir(self._base())
        cfg = load_config(d / 'pylon.toml')
        assert cfg.cache.enabled is False
        assert cfg.cache.path == (d / '.pylon' / 'cache').resolve()

    def test_cache_section_parsed(self, toml_dir):
        d = toml_dir(
            self._base()
            + """
            [cache]
            enabled = true
            max_size_mb = 2048
        """
        )
        cfg = load_config(d / 'pylon.toml')
        assert cfg.cache.enabled is True
        assert cfg.cache.max_size_mb == 2048
        assert cfg.cache.backend == 'lmdb'

    def test_cache_invalid_backend_raises(self, toml_dir):
        d = toml_dir(
            self._base()
            + """
            [cache]
            backend = "memcached"
        """
        )
        with pytest.raises(ValueError, match='lmdb'):
            load_config(d / 'pylon.toml')

    def test_cache_path_relative_resolved_against_toml_dir(self, toml_dir):
        d = toml_dir(
            self._base()
            + """
            [cache]
            path = "my-cache"
        """
        )
        cfg = load_config(d / 'pylon.toml')
        assert cfg.cache.path == (d / 'my-cache').resolve()

    def test_cache_path_tilde_expanded(self, toml_dir, monkeypatch):
        monkeypatch.setenv('HOME', '/home/testuser')
        d = toml_dir(
            self._base()
            + """
            [cache]
            path = "~/pylon-cache"
        """
        )
        cfg = load_config(d / 'pylon.toml')
        assert cfg.cache.path == Path('/home/testuser/pylon-cache')

    def test_cache_sets_override(self, toml_dir):
        d = toml_dir(
            self._base()
            + """
            [cache]
            enabled = true

            [cache.sets.Order]
            enabled = false
        """
        )
        cfg = load_config(d / 'pylon.toml')
        assert cfg.cache.sets['Order'].enabled is False

    def test_cache_set_defaults_to_enabled(self, toml_dir):
        d = toml_dir(
            self._base()
            + """
            [cache]
            enabled = true

            [cache.sets.Order]
        """
        )
        cfg = load_config(d / 'pylon.toml')
        assert cfg.cache.sets['Order'].enabled is True
