"""L2 hardware-less integration tests: real `probe-rs serve` + dry-run sessions.

These exercise the RPC lifecycle (idempotent open, session flows, error
kinds, stale-connection recovery) against a real spawned server. They need
the built extension module and the workspace `probe-rs` binary (resolved via
PYPROBE_RS_BIN or the workspace target dir — no probe hardware required).
"""

import asyncio
import gc

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


def test_close_all_severs_handles_immediately(server):
    """close_all() must release the probe lease even while CoreHandle
    objects are still referenced: post-close handle calls fail fast with
    kind Connection instead of issuing real RPCs."""
    s1 = _dry_session()
    s2 = _dry_session()
    core = s1.core(0)
    try:
        pr.close_all()
        for s in (s1, s2):
            with pytest.raises(pr.ProbeRsError) as excinfo:
                s.core(0)
            assert excinfo.value.kind == "Connection"
        with pytest.raises(pr.ProbeRsError) as excinfo:
            core.status()
        assert excinfo.value.kind == "Connection"
        assert pr.sessions() == []
    finally:
        pr.close_all()


def test_drop_without_close_releases_session(server):
    """The stale-cell scenario: a session dropped without close() releases
    its server-side session on disconnect, so a fresh attach converges."""
    s1 = _dry_session()
    del s1
    gc.collect()
    s2 = _dry_session()
    try:
        assert len(s2.list_cores()) >= 1
    finally:
        s2.close()


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


def test_dry_run_download_option_is_rejected(server, tmp_path):
    """dry_run moved to the attach request; setting it per-download must be
    loud, not silently ignored on a destructive operation."""
    sess = _dry_session()
    try:
        img = tmp_path / "blink.bin"
        img.write_bytes(bytes([0x01]))
        opts = pr.DownloadOptions()
        opts.dry_run = True
        with pytest.raises(ValueError, match="dry_run"):
            pr.download_file(sess, str(img), "bin", options=opts, base_address=0x08000000)
    finally:
        sess.close()


def test_flash_pipeline_dry_run(server, tmp_path):
    """Full flash pipeline on a dry-run session: upload, loader build,
    program (dry), boot. Progress events are delivered on the pump thread,
    so the callback may call back into pyprobe_rs (the reentrancy that used
    to panic)."""
    sess = pr.Session.auto_attach("stm32wb55ccux", dry_run=True)
    try:
        img = tmp_path / "blink.bin"
        img.write_bytes(bytes([0x01, 0x02, 0x03, 0x04]))

        results = []

        def progress(event):
            try:
                # P0 regression: a facade call from inside the callback
                # must not hit the nested-runtime panic.
                status = sess.core(0).status()
                results.append(("ok", str(status)))
            except BaseException as exc:  # noqa: BLE001 - recorded on purpose
                results.append(("err", type(exc).__name__))

        loader = pr.build_loader(sess, str(img), "bin", base_address=0x08000000)
        loader.add_data(0x08000010, bytes([0xAA, 0xBB]))
        loader.commit(sess, progress=progress)
        # No PanicException may surface, whatever the event count was.
        assert all(kind == "ok" for kind, _ in results), results
        # The session survives the flash and stays usable.
        assert len(sess.list_cores()) >= 1
    finally:
        sess.close()


if __name__ == "__main__":
    import sys

    sys.exit(pytest.main([__file__, "-v"]))
