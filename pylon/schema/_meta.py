from __future__ import annotations

import dataclasses
from typing import Any

MISSING = dataclasses.MISSING


@dataclasses.dataclass
class FieldMeta:
    """Metadata for a single field on a Pylon type."""

    name: str
    kind: str  # 'property' | 'link' | 'multilink' | 'computed'
    scalar_type: Any  # pylon scalar class, or raw Python type for shorthands
    nullable: bool
    constraints: list[Any]
    default: Any  # dataclasses.MISSING or a concrete default value
    default_factory: Any  # dataclasses.MISSING or a zero-argument callable
    description: str | None = None
    # link / multilink
    link_target: Any = None  # the linked Pylon type
    through: Any = None  # intermediate type for MultiLink with link properties
    # computed
    expression: str | None = None
    # mutation rewrites declared inside Property[T, Rewrite(...)]
    rewrites: list[Any] = dataclasses.field(default_factory=list)
    # deletion policies declared inside Link[T, OnDelete(...)] or MultiLink[T, OnDelete(...)]
    on_delete: list[Any] = dataclasses.field(default_factory=list)
    # transpiler-level read-only flag; does not affect PostgreSQL
    is_readonly: bool = False


@dataclasses.dataclass
class PylonConfig:
    """Schema metadata attached to every Pylon type as __pylon_config__."""

    module: str
    name: str
    table: str
    abstract: bool
    materialized: bool
    fields: dict[str, FieldMeta] = dataclasses.field(default_factory=dict)
    constraints: list[Any] = dataclasses.field(default_factory=list)
    indexes: list[Any] = dataclasses.field(default_factory=list)
    vector_indexes: list[Any] = dataclasses.field(default_factory=list)
    triggers: list[Any] = dataclasses.field(default_factory=list)
    description: str | None = None
    junction: bool = False
