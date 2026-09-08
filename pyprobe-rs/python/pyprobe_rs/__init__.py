"""Python bindings for the probe-rs embedded debugging toolkit.

The Rust extension drives a managed ``probe-rs serve`` process over RPC:
``open()`` converges the server state (idempotent), ``Session.auto_attach``
attaches, and every blocking call releases the GIL so the interpreter stays
responsive during flash, RTT, and resets.

``aio(obj)`` returns an asyncio view of a facade object: every method call
becomes an awaitable running in the default executor (the underlying call
releases the GIL, so the event loop is never blocked)::

    s = pyprobe_rs.Session.auto_attach("nRF52840_xxAA")
    await pyprobe_rs.aio(s).flash("app.elf")
"""

from ._pyprobe_rs import *  # noqa: F401,F403
from ._pyprobe_rs import __version__  # noqa: F401


class _Aio:
    """Asyncio view of a facade object; attribute access yields awaitables."""

    __slots__ = ("_owner",)

    def __init__(self, owner):
        object.__setattr__(self, "_owner", owner)

    def __getattr__(self, name):
        target = getattr(object.__getattribute__(self, "_owner"), name)
        if not callable(target):
            return target
        import asyncio

        def call(*args, **kwargs):
            loop = asyncio.get_running_loop()
            return loop.run_in_executor(None, lambda: target(*args, **kwargs))

        call.__name__ = name
        return call


def aio(obj):
    """Return an asyncio view of a pyprobe-rs facade object."""
    return _Aio(obj)


def _attach_aio_properties():
    import sys

    ext = sys.modules[__name__].__dict__.get("_pyprobe_rs")
    if ext is None:
        return
    prop = property(aio)
    for name in ("Session", "CoreHandle", "Rtt", "UpChannel", "DownChannel"):
        cls = getattr(ext, name, None)
        if cls is None:
            continue
        try:
            setattr(cls, "aio", prop)
        except TypeError:
            # Immutable extension type: use the module-level aio() helper.
            return


_attach_aio_properties()
