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
from .exceptions import PylonError

# ── Schema decorators ──────────────────────────────────────────────────────────
# ── Schema base types & introspection ─────────────────────────────────────────
# ── Pointer annotations ────────────────────────────────────────────────────────
# ── Constraints ────────────────────────────────────────────────────────────────
# ── Indexes ────────────────────────────────────────────────────────────────────
# ── Built-in scalars ───────────────────────────────────────────────────────────
from .schema import (
    JSON,
    UUID,
    Allow,
    Array,
    BaseObject,
    Bool,
    NamedTuple,
    Bytes,
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
    PointerMeta,
    Float32,
    Float64,
    Alias,
    AliasDescriptor,
    Channel,
    ChannelDescriptor,
    Global,
    GlobalDescriptor,
    Index,
    VectorPointer,
    VectorIndex,
    SearchBackend,
    SearchPointer,
    SearchIndex,
    SearchMode,
    SearchWeight,
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
    Now,
    OnDelete,
    OneOf,
    Readonly,
    Property,
    PylonConfig,
    Regexp,
    Restrict,
    Scalar,
    Sequence,
    On,
    SequenceNext,
    Source,
    Str,
    Target,
    Through,
    Tuple,
    Volatility,
    abstract,
    collect_module_globals,
    enum,
    function_decorator as function,
    interface,
    junction,
    lazy,
    named_tuple,
    scalar,
    signal_decorator as signal,
    type,  # shadows builtins.type intentionally  # noqa: A001
)

__all__ = [
    # Startup
    "finalize",
    # Client
    "AsyncTransaction",
    "Client",
    "Config",
    "DatabaseConfig",
    "ModelConfig",
    "PylonError",
    "SearchConfig",
    "UiConfig",
    "WebserverConfig",
    "create_async_client",
    # Schema — decorators
    "abstract",
    "enum",
    "function",
    "interface",
    "junction",
    "named_tuple",
    "scalar",
    "signal",
    "type",
    # Schema — functions
    "Language",
    "Volatility",
    # Schema — signals
    "On",
    # Schema — base types & introspection
    "BaseObject",
    "Enum",
    "NamedTuple",
    "PointerMeta",
    "Object",
    "PylonConfig",
    "Scalar",
    "lazy",
    # Schema — pointer annotations
    "Computed",
    "Link",
    "MultiLink",
    "Property",
    "Tuple",
    "Array",
    "Through",
    # Schema — deletion policies
    "OnDelete",
    "Target",
    "Source",
    "Allow",
    "Restrict",
    "DeferredRestrict",
    "DeleteSource",
    "DeleteTarget",
    "DeleteTargetIfOrphan",
    # Schema — constraints
    "Default",
    "Description",
    "Exclusive",
    "Expression",
    "MaxExValue",
    "MaxLen",
    "MaxValue",
    "MinExValue",
    "MinLen",
    "MinValue",
    "Now",
    "OneOf",
    "Readonly",
    "Regexp",
    "SequenceNext",
    # Schema — indexes
    "Index",
    "VectorPointer",
    "VectorIndex",
    "SearchBackend",
    "SearchPointer",
    "SearchIndex",
    "SearchMode",
    "SearchWeight",
    # Schema — globals
    "Alias",
    "AliasDescriptor",
    "Channel",
    "ChannelDescriptor",
    "Global",
    "GlobalDescriptor",
    "collect_module_globals",
    # Schema — built-in scalars
    "Bool",
    "Bytes",
    "DateTime",
    "Decimal",
    "Duration",
    "Float32",
    "Float64",
    "Int16",
    "Int32",
    "Int64",
    "JSON",
    "LocalDate",
    "LocalDateTime",
    "LocalTime",
    "Sequence",
    "Str",
    "UUID",
]
