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

"""Tests for the `@pylon.signal` decorator, its registry, the runtime
handler index built at `finalize()` time, and `pylon.signals`' row
hydrator — everything short of an actual live-database dispatch loop
(covered instead by `crates/pylon-core/tests/live_execution_signals.rs`
for the Rust half, and manual end-to-end verification for the full
Postgres-to-handler round trip).
"""

import uuid

import pytest

import pylon
from pylon.schema._registry import clear as clear_registry, signals_snapshot
from pylon.schema._triggers import On


@pytest.fixture(autouse=True)
def _isolated_signal_registry():
    yield
    clear_registry()


class TestSignalDecorator:
    def test_rejects_non_pylon_type_target(self):
        class NotAPylonType:
            pass

        with pytest.raises(TypeError, match="pylon.signal target must be a @pylon.type-decorated class"):
            pylon.signal(NotAPylonType)

    def test_registers_with_default_on_mask(self):
        @pylon.type(module="sigtest", name="Widget")
        class Widget:
            name: str

        @pylon.signal(Widget)
        async def handler(old, new):
            pass

        [reg] = signals_snapshot()
        assert reg.target is Widget
        assert reg.on == int(On.Insert | On.Update | On.Delete)
        assert reg.handler is handler

    def test_registers_with_explicit_on_mask(self):
        @pylon.type(module="sigtest", name="Widget2")
        class Widget2:
            name: str

        @pylon.signal(Widget2, on=On.Insert | On.Delete)
        async def handler(old, new):
            pass

        [reg] = signals_snapshot()
        assert reg.on == int(On.Insert | On.Delete)
        assert not (reg.on & int(On.Update))

    def test_decorator_returns_the_function_unchanged(self):
        @pylon.type(module="sigtest", name="Widget3")
        class Widget3:
            name: str

        async def handler(old, new):
            pass

        decorated = pylon.signal(Widget3)(handler)
        assert decorated is handler

    def test_multiple_handlers_on_same_type_all_registered(self):
        @pylon.type(module="sigtest", name="Widget4")
        class Widget4:
            name: str

        @pylon.signal(Widget4, on=On.Insert)
        async def first(old, new):
            pass

        @pylon.signal(Widget4, on=On.Delete)
        async def second(old, new):
            pass

        regs = signals_snapshot()
        assert len(regs) == 2
        assert {r.on for r in regs} == {int(On.Insert), int(On.Delete)}


class TestSignalRegistryIndex:
    def test_build_index_groups_by_type_and_operation(self):
        from pylon.schema._signal_registry import build_index
        from pylon.schema._registry import SignalRegistration

        @pylon.type(module="sigtest", name="Order")
        class Order:
            name: str

        async def on_insert(old, new):
            pass

        async def on_insert_and_delete(old, new):
            pass

        regs = [
            SignalRegistration(target=Order, on=int(On.Insert), handler=on_insert),
            SignalRegistration(target=Order, on=int(On.Insert | On.Delete), handler=on_insert_and_delete),
        ]
        index = build_index(regs)

        qname = f"{Order.__pylon_config__.module}::{Order.__name__}"
        assert set(index[qname][On.Insert]) == {on_insert, on_insert_and_delete}
        assert index[qname][On.Delete] == [on_insert_and_delete]
        assert On.Update not in index[qname]

    def test_handlers_for_unknown_type_returns_empty(self):
        from pylon.schema._signal_registry import handlers_for

        assert handlers_for("nonexistent::Type", On.Insert) == []


class TestHydrate:
    def test_none_row_yields_none(self):
        from pylon.signals import _hydrate

        @pylon.type(module="sigtest", name="Hydrated1")
        class Hydrated1:
            name: str

        assert _hydrate(Hydrated1, None) is None

    def test_hydrates_properties_and_coerces_id_to_uuid(self):
        from pylon.signals import _hydrate

        @pylon.type(module="sigtest", name="Hydrated2")
        class Hydrated2:
            name: str

        row_id = str(uuid.uuid4())
        obj = _hydrate(Hydrated2, {"id": row_id, "name": "Alpha"})

        assert isinstance(obj, Hydrated2)
        assert obj.id == uuid.UUID(row_id)
        assert obj.name == "Alpha"

    def test_hydrates_single_link_fk_column_as_uuid_not_resolved_object(self):
        from pylon.signals import _hydrate

        @pylon.type(module="sigtest", name="Org1")
        class Org1:
            name: str

        @pylon.type(module="sigtest", name="Person1")
        class Person1:
            name: str
            org: pylon.Link[Org1]

        org_id = str(uuid.uuid4())
        person_id = str(uuid.uuid4())
        obj = _hydrate(Person1, {"id": person_id, "name": "Bob", "org_id": org_id})

        assert obj.org_id == uuid.UUID(org_id)
        assert not hasattr(obj, "org")

    def test_bypasses_init(self):
        from pylon.signals import _hydrate

        @pylon.type(module="sigtest", name="Hydrated3")
        class Hydrated3:
            name: str

            def __init__(self, *args, **kwargs):
                raise AssertionError("hydration must not call __init__")

        obj = _hydrate(Hydrated3, {"id": str(uuid.uuid4()), "name": "X"})
        assert obj.name == "X"
