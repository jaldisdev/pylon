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
    from pylon.schema._globals import collect_module_globals
    from pylon.schema._registry import snapshot
    from pylon.schema._walker import walk
    from pylon.query import _set_schema

    cfg = load_config(config)
    schema_dir = cfg.project.schema_dir  # type: ignore[union-attr]
    _import_schema_dir(schema_dir)

    types, enums, custom_scalars = snapshot()

    # Collect globals from every non-private schema file that was imported.
    globals_: list[Any] = []
    for py_file in sorted(schema_dir.glob("*.py")):
        stem = py_file.stem
        if not stem.startswith("_") and stem in sys.modules:
            globals_.extend(collect_module_globals(sys.modules[stem]))
    if modules:
        for mod in modules:
            globals_.extend(collect_module_globals(mod))

    schema = walk(types, enums, custom_scalars, globals_)
    _set_schema(schema)
    return schema
