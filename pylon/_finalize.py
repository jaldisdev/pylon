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

"""pylon.finalize() — single startup call that reads pylon.toml, imports all
schema modules from the configured schema-dir, walks the registry, builds
PyO3 SchemaDescriptor objects, and installs the process-level singleton.

Usage::

    import pylon
    pylon.finalize()          # reads pylon.toml from cwd upward

    # With an explicit toml path (useful in tests / non-standard layouts):
    pylon.finalize(config="path/to/pylon.toml")
"""

from __future__ import annotations

import importlib
import importlib.util
import sys
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pylon._core import SchemaDescriptor

RESERVED_MODULE_NAMES: frozenset[str] = frozenset(
    {
        'public',
        'pg_catalog',
        'information_schema',
        'pg_toast',
    }
)


def _import_schema_dir(schema_dir: Path) -> None:
    """Import every .py file in schema_dir as a top-level module.

    schema_dir is added to the front of sys.path so that cross-module imports
    inside the schema directory work without any package prefix. Each file stem
    becomes the Python module name and therefore the Pylon module name (unless
    overridden by __pylon_module__ in the file or module= on a decorator).
    """
    schema_dir_str = str(schema_dir)
    if schema_dir_str not in sys.path:
        sys.path.insert(0, schema_dir_str)

    # Sort so __init__.py (if present) loads before sibling files.
    for py_file in sorted(schema_dir.glob('*.py')):
        stem = py_file.stem
        if stem.startswith('_'):
            continue
        if stem in RESERVED_MODULE_NAMES:
            raise ValueError(
                f"'{stem}.py' is not a valid module name: '{stem}' is a reserved "
                f'PostgreSQL schema name. Use a different name.'
            )
        if stem not in sys.modules:
            importlib.import_module(stem)


def finalize(
    *,
    config: str | Path | None = None,
    modules: list[Any] | None = None,
) -> SchemaDescriptor:
    """Walk the schema registry and build the process-level SchemaDescriptor.

    Reads ``pylon.toml`` (walking up from the current directory unless *config*
    is given), imports every ``.py`` file from ``[project] schema-dir``, then
    validates and assembles the full schema.

    Parameters
    ----------
    config:
        Explicit path to ``pylon.toml`` or its parent directory.  When omitted
        the directory tree is walked upward from ``Path.cwd()``.
    modules:
        Additional Python modules to scan for ``Global[T]`` annotations.

    Returns
    -------
    SchemaDescriptor
        The built descriptor (also installed as the process-level singleton).

    Note
    ----
    This installs the schema as currently declared in ``.py`` files on
    disk — it does not consult the database at all, and is safe to call
    with no database reachable (needed for e.g. ``pylon migration create``,
    which must build this to diff against the live DB state in the first
    place). Once a real ``Client`` connects, ``Client.ensure_connected()``
    overwrites this singleton with whatever schema the target database was
    actually last migrated to (see ``pylon.client._install_migrated_schema``)
    — so a schema change with no physical DDL footprint (e.g. a property's
    ``readonly`` flag) has no effect on query compilation until a migration
    applying it is actually run, even though this function already
    reflects it the moment the file changes.

    Raises
    ------
    FileNotFoundError
        If no ``pylon.toml`` can be located.
    KeyError
        If ``[project]`` or ``schema-dir`` is missing from the toml.
    SchemaError
        If any validation failure is detected: duplicate names, unresolvable
        link targets, required-link cycles, interface non-conformance.
    """
    from pylon.config import load_config
    from pylon.query import _set_schema
    from pylon.schema._aliases import collect_module_aliases
    from pylon.schema._channels import collect_module_channels
    from pylon.schema._globals import collect_all_globals
    from pylon.schema._registry import functions_snapshot, named_tuples_snapshot, signals_snapshot, snapshot
    from pylon.schema._signal_registry import _set_index as _set_signal_index
    from pylon.schema._signal_registry import build_index as build_signal_index
    from pylon.schema._walker import walk

    cfg = load_config(config)
    schema_dir = cfg.project.schema_dir  # type: ignore[union-attr]
    _import_schema_dir(schema_dir)

    types, enums, custom_scalars = snapshot()

    globals_: list[Any] = collect_all_globals(schema_dir, modules)

    # Collect aliases and channels from every non-private schema file that was imported.
    aliases_: list[Any] = []
    channels_: list[Any] = []
    for py_file in sorted(schema_dir.glob('*.py')):
        stem = py_file.stem
        if not stem.startswith('_') and stem in sys.modules:
            aliases_.extend(collect_module_aliases(sys.modules[stem]))
            channels_.extend(collect_module_channels(sys.modules[stem]))
    if modules:
        for mod in modules:
            aliases_.extend(collect_module_aliases(mod))
            channels_.extend(collect_module_channels(mod))

    functions = functions_snapshot()
    named_tuples = named_tuples_snapshot()
    signals = signals_snapshot()
    schema = walk(
        types,
        enums,
        custom_scalars,
        globals_,
        functions=functions,
        aliases=aliases_,
        named_tuples=named_tuples,
        signals=signals,
        channels=channels_,
    )
    _set_schema(schema)
    _set_signal_index(build_signal_index(signals))
    return schema
