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

from ._aliases import Alias, AliasDescriptor, collect_module_aliases
from ._base import BaseObject
from ._channels import Channel, ChannelDescriptor, collect_module_channels
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
from ._export import export
from ._functions import Language, Volatility
from ._functions import function as function_decorator
from ._globals import Global, GlobalDescriptor, collect_module_globals
from ._indexes import (
    Index,
    SearchBackend,
    SearchIndex,
    SearchMode,
    SearchPointer,
    SearchWeight,
    VectorIndex,
    VectorPointer,
)
from ._lazy import lazy
from ._meta import PointerMeta, PylonConfig
from ._named_tuples import NamedTuple
from ._named_tuples import named_tuple_decorator as named_tuple
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
from ._registry import named_tuples_snapshot
from ._registry import snapshot as schema_snapshot
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
from ._triggers import On, Rewrite, Timing, Trigger
from ._triggers import signal as signal_decorator
from ._walker import SchemaError

__all__ = [
    'JSON',
    'UUID',
    'Alias',
    'AliasDescriptor',
    'Allow',
    'Array',
    # Base types
    'BaseObject',
    # Built-in scalars
    'Bool',
    'Bytes',
    'Channel',
    'ChannelDescriptor',
    'Computed',
    'DateTime',
    'Decimal',
    # Constraints
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
    # Globals
    'Global',
    'GlobalDescriptor',
    # Indexes
    'Index',
    'Int16',
    'Int32',
    'Int64',
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
    'MultiLink',
    # Named tuples
    'NamedTuple',
    'Now',
    # Triggers & rewrites
    'On',
    # Deletion policies
    'OnDelete',
    'OneOf',
    # Introspection
    'PointerMeta',
    # Pointer annotations
    'Property',
    'PylonConfig',
    'Readonly',
    'Regexp',
    'Restrict',
    'Rewrite',
    'Scalar',
    # Schema validation
    'SchemaError',
    'SearchBackend',
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
    'Timing',
    'Trigger',
    'Tuple',
    'VectorIndex',
    'VectorPointer',
    # Functions
    'Volatility',
    'abstract',
    'collect_module_aliases',
    'collect_module_channels',
    'collect_module_globals',
    'enum',
    # Schema export
    'export',
    'function_decorator',
    'interface',
    'junction',
    # Lazy forward references
    'lazy',
    'named_tuple',
    'named_tuples_snapshot',
    'scalar',
    # Registry snapshot for hydration
    'schema_snapshot',
    'signal_decorator',
    # Decorators
    'type',
]
