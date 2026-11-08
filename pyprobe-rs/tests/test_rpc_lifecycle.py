"""L2 integration tests: the real `probe-rs serve` RPC lifecycle.

The server-lifecycle tests that need no session run everywhere. Tests that
need a session take the probe-gated `session` fixture from ``conftest.py``
and skip without a DUT.

probe-rs removed dry-run attach from the RPC protocol (no ``--dry-run``, no
``attach.dry_run``), so pyprobe-rs cannot mint a hardware-free session;
``test_dry_run_attach_is_rejected`` pins that contract. Core, memory, RTT,
DWARF and flash coverage that used to ride on dry-run sessions now lives in
the probe-gated HIL suite (``test_hil.py``).
"""

import asyncio
import gc

import pytest

import pyprobe_rs as pr


# ── server lifecycle (hardware-free) ────────────────────────────────────
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


def test_dry_run_attach_is_rejected(server):
    """The protocol has no fake-probe attach, so dry_run must fail loudly
    before any probe is claimed, not silently attach to real hardware."""
    with pytest.raises(pr.ProbeRsError) as excinfo:
        pr.Session.auto_attach("Cortex-M4", dry_run=True)
    assert excinfo.value.kind == "NotImplemented"
    assert "dry-run" in str(excinfo.value)
    assert pr.sessions() == []


# ── session lifecycle (probe-gated) ────────────────────────────────────
def test_close_makes_calls_fail_fast(session):
    session.close()
    session.close()  # idempotent
    with pytest.raises(pr.ProbeRsError) as excinfo:
        session.core(0)
    assert excinfo.value.kind == "Connection"


def test_close_all_severs_handles_immediately(session):
    """close_all() must release the probe lease even while CoreHandle
    objects are still referenced: post-close handle calls fail fast with
    kind Connection instead of issuing real RPCs."""
    core = session.core(0)
    pr.close_all()
    with pytest.raises(pr.ProbeRsError) as excinfo:
        session.core(0)
    assert excinfo.value.kind == "Connection"
    with pytest.raises(pr.ProbeRsError) as excinfo:
        core.status()
    assert excinfo.value.kind == "Connection"
    assert pr.sessions() == []


def test_drop_without_close_releases_session(session_factory):
    """The stale-cell scenario: a session dropped without close() releases
    its server-side session (and probe lease) on disconnect, so a fresh
    attach converges instead of being refused as ProbeBusy."""
    s1 = session_factory()
    del s1
    gc.collect()
    s2 = session_factory()
    try:
        assert len(s2.list_cores()) >= 1
    finally:
        s2.close()


def test_aio_namespace_runs_in_executor(session):
    """Two calls in flight at once: each runs on the bridge runtime with
    the GIL released, and neither blocks the other."""

    async def flow():
        core = session.core(0)
        return await asyncio.gather(
            pr.aio(core).status(),
            pr.aio(core).status(),
        )

    a, b = asyncio.run(flow())
    assert a.is_running == b.is_running


def test_download_option_dry_run_is_rejected(session, tmp_path):
    """dry_run has no wire form at all now; setting it per-download must be
    loud, not silently ignored on a destructive operation."""
    img = tmp_path / "blink.bin"
    img.write_bytes(bytes([0x01]))
    opts = pr.DownloadOptions()
    opts.dry_run = True
    with pytest.raises(ValueError, match="dry_run"):
        pr.download_file(
            session, str(img), "bin", options=opts, base_address=0x08000000
        )


if __name__ == "__main__":
    import sys

    sys.exit(pytest.main([__file__, "-v"]))
