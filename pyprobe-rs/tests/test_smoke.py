"""pyprobe-rs API smoke tests.

Hardware-free tests cover the pure API surface (version, target registry,
enum proxies, option defaults) plus the protocol contract that dry-run attach
is gone. Tests that need a session take the probe-gated `session` fixture from
``conftest.py`` and skip without a DUT; the hardware behavior itself is
covered by the HIL suite (``test_hil.py``).
"""

import re
from pathlib import Path

import pytest

import pyprobe_rs as pr


def test_version():
    """Module version must track the Cargo workspace version."""
    cargo = (Path(__file__).resolve().parents[2] / "Cargo.toml").read_text()
    match = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.MULTILINE)
    assert match, "workspace version not found in Cargo.toml"
    assert pr.__version__ == match.group(1)


def test_list_targets_includes_cortex_m4():
    targets = pr.list_targets()
    assert "Cortex-M4" in targets, f"Cortex-M4 not in {targets[:5]}..."


def test_fake_probe_attach_is_rejected():
    """A FakeProbe still yields a local handle, but the RPC protocol has no
    fake-probe attach, so attaching through it must fail loudly."""
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    with pytest.raises(pr.ProbeRsError) as excinfo:
        probe.attach("Cortex-M4", pr.Permissions())
    assert excinfo.value.kind == "NotImplemented"
    assert "dry-run" in str(excinfo.value)


def test_registry():
    reg = pr.Registry.from_builtin_families()
    families = reg.families()
    assert len(families) > 0


def test_get_target_by_name():
    t = pr.get_target_by_name("Cortex-M4")
    assert t.name == "Cortex-M4"


def test_target_cores_typed():
    t = pr.get_target_by_name("Cortex-M4")
    assert t.cores[0][1] == pr.CoreType.Armv7em


def test_download_options_ram_chunk_size():
    opts = pr.DownloadOptions()
    assert opts.ram_chunk_size is None
    opts.ram_chunk_size = 4096
    assert opts.ram_chunk_size == 4096


# ── session-dependent contracts (probe-gated) ──────────────────────────
def test_register_lookup_and_width(session):
    core = session.core(0)
    core.halt(1000)
    # Case-insensitive canonical names and common aliases resolve.
    for name in ("R0", "r0", "xpsr", "PC", "sp", "lr"):
        assert isinstance(core.read_core_reg(name), int)
    # Numeric IDs still work.
    assert isinstance(core.read_core_reg(0), int)
    # Unknown names report the valid register names (agent-grade error).
    with pytest.raises(pr.ProbeRsError) as excinfo:
        core.read_core_reg("nope")
    assert excinfo.value.kind == "Register"
    assert "known:" in str(excinfo.value)
    # Writes are validated against the register's width.
    pc = core.read_core_reg("pc")
    try:
        with pytest.raises(OverflowError):
            core.write_core_reg("R0", 1 << 32)
    finally:
        core.write_core_reg("pc", pc)


def test_setup_tracing_takes_trace_sink(session):
    # Magic strings are no longer accepted; the enum is required, and the
    # check happens before any probe work.
    with pytest.raises(TypeError):
        session.setup_tracing(0, "swo")


def test_bin_base_address_rejected_for_elf(session, tmp_path):
    elf = tmp_path / "fw.elf"
    elf.write_bytes(b"\x7fELF placeholder")
    with pytest.raises(ValueError, match="bin"):
        pr.download_file(session, str(elf), "elf", base_address=0x1000)
    with pytest.raises(ValueError, match="bin"):
        pr.build_loader(session, str(elf), "elf", skip=16)


def test_coredump_capture_roundtrip(session, tmp_path):
    core = session.core(0)
    core.halt(1000)
    # Capture below the current stack pointer: always RAM, and nothing else
    # runs while the core is halted.
    sp = core.read_core_reg("sp")
    start = (sp - 0x100) & ~0x3
    dump = pr.CoreDump.capture(session, 0, [(start, start + 0x100)])
    path = tmp_path / "dump.coredump"
    dump.store(str(path))
    loaded = pr.CoreDump.load(str(path))
    assert len(loaded.registers) > 0
    assert isinstance(loaded.data, list)


if __name__ == "__main__":
    import tempfile

    tmp = Path(tempfile.mkdtemp())
    test_version()
    print("✓ version")
    test_list_targets_includes_cortex_m4()
    print("✓ list_targets")
    test_fake_probe_attach_is_rejected()
    print("✓ fake probe attach rejected")
    test_registry()
    print("✓ registry")
    test_get_target_by_name()
    print("✓ get_target_by_name")
    test_target_cores_typed()
    print("✓ target cores typed")
    test_download_options_ram_chunk_size()
    print("✓ download options ram_chunk_size")
    print("\nAll hardware-free tests passed! (session tests need PYPROBE_RS_PY_HIL_CONFIG)")
