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

from ._finalize import finalize
from .client import AsyncTransaction, Client, create_async_client
from .config import Config, DatabaseConfig, ModelConfig, SearchConfig, UiConfig, WebserverConfig
from .datatypes import Object
from .exceptions import PylonError, Rollback

# ── PyQL stdlib namespaces ─────────────────────────────────────────────────────
# `from pylon import std` — usable both in query-builder expressions
# (`User.filter(lambda u: std.ilike(u.name, '%bob%'))`) and as a pointer
# default (`Default(std.uuid_generate_v7())`). `math` deliberately shadows
# nothing: it is only ever reached via this import, never as a bare name.
from .namespaces import cal, math, std, sys

# ── Schema decorators ──────────────────────────────────────────────────────────
# ── Schema base types & introspection ─────────────────────────────────────────
# ── Pointer annotations ────────────────────────────────────────────────────────
# ── Constraints ────────────────────────────────────────────────────────────────
# ── Indexes ────────────────────────────────────────────────────────────────────
# ── Built-in scalars ───────────────────────────────────────────────────────────
from .schema import (
    JSON,
    UUID,
    Alias,
    AliasDescriptor,
    Allow,
    Array,
    BaseObject,
    Bool,
    Bytes,
    Channel,
    ChannelDescriptor,
    Computed,
    DateTime,
    Decimal,
    Default,
    DeferredRestrict,
    DeleteSource,
    DeleteTarget,
    DeleteTargetIfOrphan,
    Description,
    Duration,
    Enum,
    Exclusive,
    Expression,
    Float32,
    Float64,
    Global,
    GlobalDescriptor,
    Index,
    Int16,
    Int32,
    Int64,
    Language,
    Link,
    LocalDate,
    LocalDateTime,
    LocalTime,
    MaxExValue,
    MaxLen,
    MaxValue,
    MinExValue,
    MinLen,
    MinValue,
    MultiLink,
    NamedTuple,
    Now,
    On,
    OnDelete,
    OneOf,
    Partition,
    PointerMeta,
    Property,
    PylonConfig,
    Readonly,
    Regexp,
    Restrict,
    Rewrite,
    Scalar,
    SearchBackend,
    SearchIndex,
    SearchMode,
    SearchPointer,
    SearchWeight,
    Sequence,
    SequenceNext,
    Source,
    Str,
    Target,
    Through,
    Tuple,
    VectorIndex,
    VectorPointer,
    Volatility,
    abstract,
    collect_module_globals,
    enum,
    interface,
    junction,
    lazy,
    named_tuple,
    scalar,
    type,  # shadows builtins.type intentionally
    Timing,
    Trigger,
)
from .schema import (
    function_decorator as function,
)
from .schema import (
    signal_decorator as signal,
)

__all__ = [
    'JSON',
    'UUID',
    # Schema — globals
    'Alias',
    'AliasDescriptor',
    'Allow',
    'Array',
    # Client
    'AsyncTransaction',
    # Schema — base types & introspection
    'BaseObject',
    # Schema — built-in scalars
    'Bool',
    'Bytes',
    'Channel',
    'ChannelDescriptor',
    'Client',
    # Schema — pointer annotations
    'Computed',
    'Config',
    'DatabaseConfig',
    'DateTime',
    'Decimal',
    # Schema — constraints
    'Default',
    'DeferredRestrict',
    'DeleteSource',
    'DeleteTarget',
    'DeleteTargetIfOrphan',
    'Description',
    'Duration',
    'Enum',
    'Exclusive',
    'Expression',
    'Float32',
    'Float64',
    'Global',
    'GlobalDescriptor',
    # Schema — indexes
    'Index',
    'Int16',
    'Int32',
    'Int64',
    # Schema — functions
    'Language',
    'Link',
    'LocalDate',
    'LocalDateTime',
    'LocalTime',
    'MaxExValue',
    'MaxLen',
    'MaxValue',
    'MinExValue',
    'MinLen',
    'MinValue',
    'ModelConfig',
    'MultiLink',
    'NamedTuple',
    'Now',
    'Object',
    # Schema — signals
    'On',
    # Schema — deletion policies
    'OnDelete',
    'OneOf',
    'Partition',
    'PointerMeta',
    'Property',
    'PylonConfig',
    'PylonError',
    'Readonly',
    'Rewrite',
    'Timing',
    'Trigger',
    'Regexp',
    'Restrict',
    'Rollback',
    'Scalar',
    'SearchBackend',
    'SearchConfig',
    'SearchIndex',
    'SearchMode',
    'SearchPointer',
    'SearchWeight',
    'Sequence',
    'SequenceNext',
    'Source',
    'Str',
    'Target',
    'Through',
    'Tuple',
    'UiConfig',
    'VectorIndex',
    'VectorPointer',
    'Volatility',
    'WebserverConfig',
    # Schema — decorators
    'abstract',
    # PyQL stdlib namespaces
    'cal',
    'collect_module_globals',
    'create_async_client',
    'enum',
    # Startup
    'finalize',
    'function',
    'interface',
    'junction',
    'lazy',
    'math',
    'named_tuple',
    'scalar',
    'signal',
    'std',
    'sys',
    'type',
]
