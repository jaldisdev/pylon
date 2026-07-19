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

RESERVED_MODULE_NAMES: frozenset[str] = frozenset({
    "public", "pg_catalog", "information_schema", "pg_toast",
})


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
    for py_file in sorted(schema_dir.glob("*.py")):
        stem = py_file.stem
        if stem.startswith("_"):
            continue
        if stem in RESERVED_MODULE_NAMES:
            raise ValueError(
                f"'{stem}.py' is not a valid module name: '{stem}' is a reserved "
                f"PostgreSQL schema name. Use a different name."
            )
        if stem not in sys.modules:
            importlib.import_module(stem)


def finalize(
    *,
    config: str | Path | None = None,
    modules: list[Any] | None = None,
) -> "SchemaDescriptor":
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
    from pylon.schema._globals import collect_all_globals
    from pylon.schema._aliases import collect_module_aliases
    from pylon.schema._registry import snapshot, functions_snapshot, named_tuples_snapshot, signals_snapshot
    from pylon.schema._walker import walk
    from pylon.schema._signal_registry import build_index as build_signal_index, _set_index as _set_signal_index
    from pylon.query import _set_schema

    cfg = load_config(config)
    schema_dir = cfg.project.schema_dir  # type: ignore[union-attr]
    _import_schema_dir(schema_dir)

    types, enums, custom_scalars = snapshot()

    globals_: list[Any] = collect_all_globals(schema_dir, modules)

    # Collect aliases from every non-private schema file that was imported.
    aliases_: list[Any] = []
    for py_file in sorted(schema_dir.glob("*.py")):
        stem = py_file.stem
        if not stem.startswith("_") and stem in sys.modules:
            aliases_.extend(collect_module_aliases(sys.modules[stem]))
    if modules:
        for mod in modules:
            aliases_.extend(collect_module_aliases(mod))

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
    )
    _set_schema(schema)
    _set_signal_index(build_signal_index(signals))
    _export_schema_json(cfg, schema)
    return schema


def _export_schema_json(cfg: Any, schema: "SchemaDescriptor") -> None:
    """Write the schema to ``.pylon/schema.json`` next to ``pylon.toml``.

    Lets `pylon-lsp` (a pure-Rust binary with no embedded Python) load the
    same schema this process just built, so it can run the full compiler
    and surface semantic diagnostics instead of only parser errors.
    """
    if cfg.toml_path is None:
        return
    pylon_dir = cfg.toml_path.parent / ".pylon"
    pylon_dir.mkdir(exist_ok=True)
    (pylon_dir / "schema.json").write_text(schema.to_json())
