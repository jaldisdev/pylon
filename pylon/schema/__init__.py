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

from ._base import BaseObject
from ._export import export
from ._walker import SchemaError
from ._globals import Global, GlobalDescriptor, collect_module_globals
from ._aliases import Alias, AliasDescriptor, collect_module_aliases
from ._constraints import (
    Default,
    Description,
    Exclusive,
    Expression,
    MaxExValue,
    MaxLen,
    MaxValue,
    MinExValue,
    MinLen,
    MinValue,
    Now,
    OneOf,
    Readonly,
    Regexp,
    SequenceNext,
)
from ._decorators import (
    abstract_decorator as abstract,
)
from ._decorators import (
    interface_decorator as interface,
)
from ._decorators import (
    junction_decorator as junction,
)
from ._decorators import (
    type_decorator as type,
)
from ._enums import Enum
from ._enums import enum_decorator as enum
from ._named_tuples import NamedTuple
from ._named_tuples import named_tuple_decorator as named_tuple
from ._registry import named_tuples_snapshot
from ._pointers import (
    Allow,
    Array,
    Computed,
    DeferredRestrict,
    DeleteSource,
    DeleteTarget,
    DeleteTargetIfOrphan,
    Link,
    MultiLink,
    OnDelete,
    Property,
    Restrict,
    Source,
    Target,
    Through,
    Tuple,
)
from ._indexes import Index, SearchBackend, SearchPointer, SearchIndex, SearchMode, SearchWeight, VectorPointer, VectorIndex
from ._lazy import lazy
from ._registry import snapshot as schema_snapshot
from ._functions import Volatility, Language, function as function_decorator
from ._triggers import On, Rewrite, Timing, Trigger, signal as signal_decorator
from ._meta import PointerMeta, PylonConfig
from ._scalars import (
    JSON,
    UUID,
    Bool,
    Bytes,
    DateTime,
    Decimal,
    Duration,
    Float32,
    Float64,
    Int16,
    Int32,
    Int64,
    LocalDate,
    LocalDateTime,
    LocalTime,
    Scalar,
    Sequence,
    Str,
    scalar,
)

__all__ = [
    # Decorators
    "type",
    "abstract",
    "interface",
    "enum",
    "scalar",
    "function_decorator",
    "signal_decorator",
    # Functions
    "Volatility",
    "Language",
    # Base types
    "BaseObject",
    "Scalar",
    "Enum",
    # Pointer annotations
    "Property",
    "Link",
    "MultiLink",
    "Computed",
    "Tuple",
    "Array",
    "Through",
    # Deletion policies
    "OnDelete",
    "Target",
    "Source",
    "Allow",
    "Restrict",
    "DeferredRestrict",
    "DeleteSource",
    "DeleteTarget",
    "DeleteTargetIfOrphan",
    # Constraints
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
    # Indexes
    "Index",
    "VectorPointer",
    "VectorIndex",
    "SearchBackend",
    "SearchPointer",
    "SearchIndex",
    "SearchMode",
    "SearchWeight",
    # Triggers & rewrites
    "On",
    "Timing",
    "Trigger",
    "Rewrite",
    # Built-in scalars
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
    "Str",
    "UUID",
    # Named tuples
    "NamedTuple",
    "named_tuple",
    "named_tuples_snapshot",
    # Introspection
    "PointerMeta",
    "PylonConfig",
    # Lazy forward references
    "lazy",
    # Schema export
    "export",
    # Globals
    "Global",
    "GlobalDescriptor",
    "collect_module_globals",
    # Schema validation
    "SchemaError",
    # Registry snapshot for hydration
    "schema_snapshot",
]
