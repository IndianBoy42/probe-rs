"""Shared fixtures for the pyprobe-rs Python suites.

Sessions are RPC-managed and the RPC protocol has no fake-probe attach
(probe-rs removed `--dry-run` and `attach.dry_run`), so any test that needs a
session needs a real DUT. Those tests read ``PYPROBE_RS_PY_HIL_CONFIG`` — a
TOML in the upstream smoke-tester schema, the same file ``test_hil.py``
uses — and skip without it. Tests that only exercise the API surface stay
hardware-free.
"""

import os
import tomllib
from pathlib import Path

import pytest

import pyprobe_rs as pr

ATTACH_WAIT_S = 10.0  # probe-lease queue window for auto_attach


@pytest.fixture(scope="module")
def server():
    """The managed `probe-rs serve` process, started on demand."""
    pr.open()
    yield
    pr.close_all()
    try:
        pr.shutdown_server()
    except Exception:
        # Foreign/already-gone servers are fine at teardown.
        pass


@pytest.fixture(scope="module")
def dut() -> dict:
    """The DUT TOML, or a skip explaining how to provide one."""
    env = os.environ.get("PYPROBE_RS_PY_HIL_CONFIG")
    if not env:
        pytest.skip(
            "this test needs a DUT: set PYPROBE_RS_PY_HIL_CONFIG to a DUT TOML "
            "(smoke-tester schema: chip, probe_selector, probe_speed, protocol, "
            "reset_connected, flash_test_binary)"
        )
    path = Path(env)
    if not path.is_file():
        pytest.skip(f"PYPROBE_RS_PY_HIL_CONFIG points at a missing file: {path}")
    raw = tomllib.loads(path.read_text())
    if "chip" not in raw:
        pytest.fail(f"DUT config {path} has no 'chip' key")
    return raw


def attach(raw: dict) -> pr.Session:
    """Attach to the DUT described by `raw`, honoring its probe settings."""
    config = pr.SessionConfig()
    if raw.get("probe_speed") is not None:
        config.speed = raw["probe_speed"]
    if raw.get("protocol"):
        # DUT TOML protocol is case-insensitive ("swd"/"jtag").
        config.protocol = getattr(pr.WireProtocol, raw["protocol"].capitalize())
    return pr.Session.auto_attach(
        raw["chip"],
        config,
        wait_for_probe=ATTACH_WAIT_S,
        connect_under_reset=raw.get("reset_connected", False),
    )


@pytest.fixture()
def session_factory(server, dut):
    """Attach on demand. The DUT is shared, so the probe lease is serial:
    callers must close (or drop) each session before attaching again."""
    return lambda: attach(dut)


@pytest.fixture()
def session(session_factory) -> pr.Session:
    """One real attach per test: the only session kind the protocol offers."""
    session = session_factory()
    yield session
    session.close()
