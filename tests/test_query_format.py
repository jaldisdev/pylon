"""Tests for the REPL/CLI result formatter in pylon.cli.commands.query —
specifically that internal `__pylon_*` bookkeeping attributes (the
`__pylon_saved__` diffing shadow `Client.save()` uses, in particular) never
leak into printed output for nested dataclass values.
"""

import re

from pylon.cli.commands.query import _pformat_value, _value

_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def _strip_ansi(s: str) -> str:
    return _ANSI_RE.sub("", s)


def _make_nested_company(company_cls):
    obj = object.__new__(company_cls)
    obj.__dict__.update({
        "__pylon_type__": "default::Company",
        "id": "019f5af3-6dd9-7f1f-82d4-1846d7fa964d",
        "__pylon_saved__": {"id": "019f5af3-6dd9-7f1f-82d4-1846d7fa964d"},
    })
    return obj


class TestNestedDataclassFormatting:
    def test_pformat_value_excludes_pylon_saved(self):
        import dataclasses

        @dataclasses.dataclass
        class Company:
            pass

        obj = _make_nested_company(Company)
        out = _strip_ansi(_pformat_value(obj, depth=1, max_width=100))
        assert "__pylon_saved__" not in out
        assert "__pylon_type__" not in out
        assert "id:" in out

    def test_value_excludes_pylon_saved(self):
        import dataclasses

        @dataclasses.dataclass
        class Company:
            pass

        obj = _make_nested_company(Company)
        out = _strip_ansi(_value(obj))
        assert "__pylon_saved__" not in out
        assert "__pylon_type__" not in out
        assert "id:" in out
