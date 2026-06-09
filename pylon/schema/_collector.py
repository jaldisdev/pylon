import threading

_local = threading.local()


def _list() -> list[object]:
    if not hasattr(_local, "items"):
        _local.items = []
    return _local.items


def register(expr: object) -> None:
    """Append expr to the thread-local pending list."""
    _list().append(expr)


def unregister(expr: object) -> None:
    """Remove expr from the pending list; no-op if not present."""
    try:
        _list().remove(expr)
    except ValueError:
        pass


def drain() -> list[object]:
    """Return all pending expressions and clear the list."""
    items = list(_list())
    _list().clear()
    return items
