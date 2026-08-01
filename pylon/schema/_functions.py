"""User-defined function decorator and related types."""

from __future__ import annotations

import inspect
import sys
from typing import Any


class Volatility:
    Immutable = "immutable"
    Stable = "stable"
    Volatile = "volatile"
    Modifying = "volatile"  # PostgreSQL maps this to VOLATILE


_VALID_VOLATILITIES = {Volatility.Immutable, Volatility.Stable, Volatility.Volatile}


class Language:
    PyQL = "pyql"


class _PylonFunctionConfig:
    __slots__ = ("func", "name", "module", "language", "volatility", "body")

    def __init__(
        self,
        func: Any,
        name: str | None,
        module: str | None,
        language: str,
        volatility: str | None,
    ) -> None:
        self.func = func
        self.name = name or func.__name__
        self.module = module or _infer_module(func)
        self.language = language
        self.volatility = volatility
        self.body = inspect.getdoc(func) or ""


def _infer_module(func: Any) -> str:
    defining = sys.modules.get(func.__module__ or "")
    if defining is not None:
        override = getattr(defining, "__pylon_module__", None)
        if isinstance(override, str):
            return override
    module_path = func.__module__ or "default"
    return module_path.rpartition(".")[-1] or module_path


def function(
    _func: Any = None,
    *,
    name: str | None = None,
    language: str = Language.PyQL,
    volatility: str | None = None,
    module: str | None = None,
) -> Any:
    """Decorator to define a user-defined Pylon function.

    Usage::

        @pylon.function
        def mysum(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
            '''select a + b'''

        @pylon.function(volatility=pylon.Volatility.Immutable)
        def mysum(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
            '''select a + b'''
    """

    def decorator(func: Any) -> Any:
        # Deferred to dodge the walker/functions import cycle (_walker imports
        # Volatility from this module; see the matching deferred imports
        # throughout _walker.py for the same reason).
        from ._walker import SchemaError

        config = _PylonFunctionConfig(
            func=func,
            name=name,
            module=module,
            language=language,
            volatility=volatility,
        )
        qname = f"{config.module}::{config.name}"

        if volatility is not None and volatility not in _VALID_VOLATILITIES:
            raise SchemaError(
                f"function {qname!r}: invalid volatility {volatility!r}, expected one "
                f"of 'immutable', 'stable', 'volatile'"
            )
        if language != Language.PyQL:
            raise SchemaError(
                f"function {qname!r}: invalid language {language!r}, only "
                f"{Language.PyQL!r} is currently supported"
            )

        func.__pylon_function__ = config
        from ._registry import register_function
        register_function(func)
        return func

    if _func is not None:
        return decorator(_func)
    return decorator
