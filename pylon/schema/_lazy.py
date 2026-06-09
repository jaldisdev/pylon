from __future__ import annotations


class _Lazy:
    """Annotated metadata marker for resolving a forward-referenced type.

    The module_path is a dot-prefixed path relative to the defining module.
    Pylon resolves the string at schema build time using this path as the
    import anchor.
    """

    __slots__ = ("module_path",)

    def __init__(self, module_path: str) -> None:
        self.module_path = module_path

    def __repr__(self) -> str:
        return f"pylon.lazy({self.module_path!r})"


def lazy(module_path: str) -> _Lazy:
    """Break circular imports in link annotations.

    Usage::

        from __future__ import annotations
        from typing import Annotated
        import pylon

        @pylon.type
        class Order:
            product: Link[Annotated['Product', pylon.lazy('.product')]]

    The string ``'Product'`` is a forward reference. pylon.lazy supplies the
    module path so the schema builder can locate the type without requiring
    the import to be resolved at class-definition time.
    """
    return _Lazy(module_path)
