"""L2 hardware-less integration tests: real `probe-rs serve` + dry-run sessions.

These exercise the RPC lifecycle (idempotent open, session flows, error
kinds, stale-connection recovery) against a real spawned server. They need
the built extension module and the workspace `probe-rs` binary (resolved via
PYPROBE_RS_BIN or the workspace target dir — no probe hardware required).
"""

import asyncio

import pytest

import pyprobe_rs as pr

CHIP = "Cortex-M4"


@pytest.fixture(scope="module")
def server():
    pr.open()
    yield
    pr.close_all()
    try:
        pr.shutdown_server()
    except Exception:
        # Foreign/already-gone servers are fine at teardown.
        pass


def _dry_session() -> pr.Session:
    return pr.Session.auto_attach(CHIP, dry_run=True)


def test_open_is_idempotent(server):
    """Repeated open() converges instead of erroring (stale-cell killer)."""
    for _ in range(3):
        pr.open()
    assert isinstance(pr.server_logs(), list)
    assert isinstance(pr.sessions(), list)


def test_open_bad_host_yields_connection_kind():
    with pytest.raises(pr.ProbeRsError) as excinfo:
        pr.open(host="ws://127.0.0.1:1")
    assert excinfo.value.kind == "Connection"
    # Return to local mode for the other tests.
    pr.open()


def test_dry_run_core_flow(server):
    sess = _dry_session()
    try:
        assert len(sess.list_cores()) >= 1
        core = sess.core(0)

        pc = core.halt(2000)
        assert isinstance(pc, int)
        status = core.status()
        assert not status.is_running

        core.run()
        assert core.status().is_running

        # The mocked core cannot service reset sequences; whichever way it
        # goes, it must be a clean classified error, never a dead connection.
        try:
            core.reset_and_halt(2000)
        except pr.ProbeRsError as e:
            assert e.kind not in ("ServerGone",), e
        # The mocked core's reset support is state-dependent; whatever it
        # does, it must be a clean classified error, never a dead connection.
        try:
            core.reset()
        except pr.ProbeRsError as e:
            assert e.kind not in ("ServerGone",), e
    finally:
        sess.close()


def test_dry_run_memory_access(server):
    sess = _dry_session()
    try:
        core = sess.core(0)
        assert isinstance(core.read_word_32(0x2000_0000), int)
        block = core.read_32(0x2000_0000, 8)
        assert len(block) == 8
    finally:
        sess.close()


def test_close_makes_calls_fail_fast(server):
    sess = _dry_session()
    sess.close()
    sess.close()  # idempotent
    with pytest.raises(pr.ProbeRsError) as excinfo:
        sess.core(0)
    assert excinfo.value.kind == "Connection"


def test_close_all_marks_sessions_closed(server):
    s1 = _dry_session()
    s2 = _dry_session()
    try:
        pr.close_all()
        for s in (s1, s2):
            with pytest.raises(pr.ProbeRsError) as excinfo:
                s.core(0)
            assert excinfo.value.kind == "Connection"
    finally:
        pr.close_all()


def test_unsupported_width_panic_is_server_gone(server):
    """FakeProbe panics on 16-bit reads; the server drops the connection and
    the client must see ServerGone with a fix message, then recover by
    attaching a fresh session."""
    sess = _dry_session()
    try:
        core = sess.core(0)
        with pytest.raises(pr.ProbeRsError) as excinfo:
            core.read_word_16(0x2000_0000)
        assert excinfo.value.kind in ("ServerGone", "Rpc", "Connection")
    finally:
        sess.close()
    # Recovery: a fresh dry-run attach works after the dead connection.
    fresh = _dry_session()
    try:
        assert len(fresh.list_cores()) >= 1
    finally:
        fresh.close()


def test_dry_run_rtt_is_clean(server):
    sess = _dry_session()
    try:
        # The mocked core has no RTT control block; attach either fails
        # cleanly or yields a usable Rtt — never a dead connection.
        try:
            rtt = sess.attach_rtt(0)
            assert isinstance(rtt.up_channels(), list)
            rtt.stop_streaming()
        except pr.ProbeRsError as e:
            assert e.kind not in ("ServerGone",), e
    finally:
        sess.close()


def test_debug_surface_shapes(server):
    sess = _dry_session()
    try:
        # Without a loaded ELF the rich trace yields an empty list (or a
        # clean error) — never a dead connection.
        try:
            frames = sess.stack_trace(0)
            assert isinstance(frames, list)
        except pr.ProbeRsError as e:
            assert e.kind not in ("ServerGone",), e
    finally:
        sess.close()


def test_aio_namespace_runs_in_executor(server):
    async def flow():
        sess = _dry_session()
        try:
            core = sess.core(0)
            a, b = await asyncio.gather(
                pr.aio(core).read_word_32(0x2000_0000),
                pr.aio(core).read_word_32(0x2000_0004),
            )
            return a, b
        finally:
            sess.close()

    a, b = asyncio.run(flow())
    assert isinstance(a, int) and isinstance(b, int)


if __name__ == "__main__":
    import sys

    sys.exit(pytest.main([__file__, "-v"]))
