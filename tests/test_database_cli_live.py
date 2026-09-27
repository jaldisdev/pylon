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

"""Live-Postgres tests for `pylon database`.

`wipe` empties the whole database it is pointed at, this file included — it
leaves nothing of any other test's modules behind, so run it on its own
database or after the other live suites.

Run with: `.venv/bin/pytest tests/test_database_cli_live.py -m live_db`
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
from click.testing import CliRunner
from conftest import live_db_dsn

from pylon.cli.commands.database import wipe
from pylon.config import Config, DatabaseConfig, ProjectConfig

pytestmark = [pytest.mark.live_db]


# Objects an extension owns are not Pylon's to drop, and dropping one takes the
# whole extension with it. Counted before and after, per catalog.
_EXTENSION_OBJECTS = """
SELECT (
    (SELECT count(*) FROM pg_extension)::text || ' ' ||
    (SELECT count(*) FROM pg_depend d
      WHERE d.deptype = 'e' AND d.classid IN ('pg_proc'::regclass, 'pg_class'::regclass, 'pg_type'::regclass))::text
) AS result
"""

# Anything left in `public` that no extension owns, plus any module schema.
_LEFTOVERS = """
SELECT (kind || ' ' || name) AS result FROM (
    SELECT c.relkind::text AS kind, c.relname AS name
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'public'
      AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
      AND NOT EXISTS (
          SELECT 1 FROM pg_depend d
          WHERE d.objid = c.oid AND d.classid = 'pg_class'::regclass AND d.deptype = 'e'
      )
    UNION ALL
    SELECT 'type', t.typname
    FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
    WHERE n.nspname = 'public'
      AND t.typtype IN ('e', 'd', 'r', 'c')
      AND NOT EXISTS (
          SELECT 1 FROM pg_depend d
          WHERE d.objid = t.oid AND d.classid = 'pg_type'::regclass AND d.deptype IN ('e', 'i')
      )
    UNION ALL
    SELECT 'func', p.proname
    FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
    WHERE n.nspname = 'public'
      AND NOT EXISTS (
          SELECT 1 FROM pg_depend d
          WHERE d.objid = p.oid AND d.classid = 'pg_proc'::regclass AND d.deptype = 'e'
      )
    UNION ALL
    SELECT 'schema', schema_name FROM information_schema.schemata
    WHERE schema_name NOT IN ('information_schema', 'public', '_pylon')
      AND schema_name NOT LIKE 'pg\\_%'
) AS leftovers
"""

_INTERNAL_ROWS = """
SELECT (
    (SELECT count(*) FROM _pylon."Migrations") +
    (SELECT count(*) FROM _pylon."Progress") +
    (SELECT count(*) FROM _pylon."Schema") +
    (SELECT count(*) FROM _pylon."SignalOutbox") +
    (SELECT count(*) FROM _pylon."IndexOutbox")
) AS result
"""


def _run_wipe() -> None:
    config = Config(
        database=DatabaseConfig(dsn=live_db_dsn()),
        project=ProjectConfig(schema_dir=Path('.')),
    )
    result = CliRunner().invoke(wipe, ['--force'], obj={'config': config})
    assert result.exit_code == 0, result.output


def test_wipe_clears_public_and_keeps_extensions(live_pool, unique_module):
    module = unique_module('live_wipe')

    # Start from a known-empty `public`, so a leftover from an earlier suite
    # cannot collide with the fixtures below.
    _run_wipe()

    async def setup():
        # The `default` module's shapes as they land in `public`: an object
        # table, its multilink junction table, an enum and a domain the table
        # depends on, a view over it, a trigger guard function, a partitioned
        # table, and a sequence. Plus a second module, for the plain
        # DROP SCHEMA path.
        await live_pool.batch_execute(f"""
            CREATE SCHEMA "{module}";
            CREATE TABLE "{module}"."Thing" (id uuid PRIMARY KEY DEFAULT uuidv7());
            CREATE TYPE public."WipeGender" AS ENUM ('Male', 'Female');
            CREATE DOMAIN public.wipe_pos_int AS integer CHECK (VALUE > 0);
            CREATE TABLE public."WipePerson" (
                id uuid PRIMARY KEY DEFAULT uuidv7(),
                gender public."WipeGender",
                age public.wipe_pos_int,
                counted serial
            );
            CREATE TABLE public."WipePerson.posts" (
                source uuid NOT NULL REFERENCES public."WipePerson"(id),
                target uuid NOT NULL
            );
            CREATE TABLE public."WipePart" (id uuid, k int, PRIMARY KEY (id, k)) PARTITION BY RANGE (k);
            CREATE TABLE public."WipePart_p1" PARTITION OF public."WipePart" FOR VALUES FROM (0) TO (10);
            CREATE VIEW public."WipePersonView" AS SELECT id FROM public."WipePerson";
            CREATE MATERIALIZED VIEW public."WipePersonMat" AS SELECT id FROM public."WipePerson";
            CREATE SEQUENCE public.wipe_standalone_seq;
            CREATE FUNCTION public.wipe_guard() RETURNS trigger LANGUAGE plpgsql AS $$
                BEGIN RETURN NEW; END $$;
            CREATE TRIGGER wipe_guard AFTER INSERT ON public."WipePerson"
                FOR EACH ROW EXECUTE FUNCTION public.wipe_guard();
            INSERT INTO _pylon."Schema" (singleton, snapshot) VALUES (true, '{{"stale": true}}')
                ON CONFLICT (singleton) DO UPDATE SET snapshot = '{{"stale": true}}';
            INSERT INTO _pylon."Migrations" (id, onto, filename) VALUES ('wipe_m1', 'initial', '00001_wipe.sql');
            INSERT INTO _pylon."Progress" (id, step_index) VALUES ('wipe_m1', 3);
            INSERT INTO _pylon."SignalOutbox" (type_name, operation) VALUES ('default::WipePerson', 'insert');
            INSERT INTO _pylon."IndexOutbox" (object_id, type_name, index_kind)
                VALUES (uuidv7(), 'default::WipePerson', 'Vector');
        """)
        return (await live_pool.query(_EXTENSION_OBJECTS, []))[0]

    extensions_before = asyncio.run(setup())

    _run_wipe()

    async def check():
        return (
            await live_pool.query(_LEFTOVERS, []),
            (await live_pool.query(_EXTENSION_OBJECTS, []))[0],
            (await live_pool.query(_INTERNAL_ROWS, []))[0],
        )

    leftovers, extensions_after, internal_rows = asyncio.run(check())

    assert leftovers == [], leftovers
    assert extensions_after == extensions_before
    assert internal_rows == 0


def test_wipe_is_idempotent(live_pool):
    """A second wipe finds nothing to drop and still succeeds."""
    _run_wipe()
    _run_wipe()
