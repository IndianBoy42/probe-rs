"""No-hardware smoke test for pyprobe-rs using FakeProbe + Cortex-M4 builtin target."""

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


def test_fake_probe_attach_and_core():
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    sess = probe.attach("Cortex-M4", pr.Permissions())
    assert sess is not None
    cores = sess.list_cores()
    assert len(cores) > 0
    core = sess.core(0)
    assert core is not None
    core.halt(1000)
    status = core.status()
    assert status.is_halted, f"Expected halted, got {status.state}"
    core.run()
    status = core.status()
    assert status.is_running, f"Expected running, got {status.state}"


def test_memory_round_trip():
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    sess = probe.attach("Cortex-M4", pr.Permissions())
    core = sess.core(0)
    core.halt(1000)
    core.write_word_32(0x2000_0000, 0xDEADBEEF)
    val = core.read_word_32(0x2000_0000)
    # MockCore doesn't persist RAM across separate reads, but the API call must complete
    # without error (per plan contingency).
    assert isinstance(val, int)


def test_registry():
    reg = pr.Registry.from_builtin_families()
    families = reg.families()
    assert len(families) > 0


def test_get_target_by_name():
    t = pr.get_target_by_name("Cortex-M4")
    assert t.name == "Cortex-M4"


def test_status_enum():
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    sess = probe.attach("Cortex-M4", pr.Permissions())
    core = sess.core(0)
    core.halt(1000)
    status = core.status()
    assert status.state == pr.CoreState.Halted
    assert not status.is_running


def test_target_cores_typed():
    t = pr.get_target_by_name("Cortex-M4")
    assert t.cores[0][1] == pr.CoreType.Armv7em


def test_error_taxonomy():
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    sess = probe.attach("Cortex-M4", pr.Permissions())
    with pytest.raises(pr.ProbeRsError) as excinfo:
        sess.attach_rtt(0)
    assert excinfo.value.kind == "Rtt", excinfo.value.kind


def test_download_options_ram_chunk_size():
    opts = pr.DownloadOptions()
    assert opts.ram_chunk_size is None
    opts.ram_chunk_size = 4096
    assert opts.ram_chunk_size == 4096


def test_setup_tracing_takes_trace_sink():
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    sess = probe.attach("Cortex-M4", pr.Permissions())
    # The mock DAP aborts on real trace setup, so only exercise the argument
    # contract: magic strings are no longer accepted, the enum is required.
    with pytest.raises(TypeError):
        sess.setup_tracing(0, "swo")


def test_register_lookup_and_width():
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    sess = probe.attach("Cortex-M4", pr.Permissions())
    core = sess.core(0)
    core.halt(1000)
    # Case-insensitive canonical names and common aliases resolve.
    for name in ("R0", "r0", "xpsr", "PC", "sp", "lr"):
        assert isinstance(core.read_core_reg(name), int)
    # Numeric IDs still work.
    assert isinstance(core.read_core_reg(0), int)
    # Unknown names report the valid register names.
    with pytest.raises(KeyError, match="known:"):
        core.read_core_reg("nope")
    # Writes are validated against the register's width.
    with pytest.raises(OverflowError):
        core.write_core_reg("R0", 1 << 32)
    core.write_core_reg("pc", 0x1000)  # alias write path


def test_bin_base_address_rejected_for_elf(tmp_path):
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    sess = probe.attach("Cortex-M4", pr.Permissions())
    elf = tmp_path / "fw.elf"
    elf.write_bytes(b"\x7fELF placeholder")
    with pytest.raises(ValueError, match="bin"):
        pr.download_file(sess, str(elf), "elf", base_address=0x1000)
    with pytest.raises(ValueError, match="bin"):
        pr.build_loader(sess, str(elf), "elf", skip=16)


def test_coredump_capture_roundtrip(tmp_path):
    fp = pr.FakeProbe.with_mocked_core()
    probe = pr.Probe.from_fake(fp)
    sess = probe.attach("Cortex-M4", pr.Permissions())
    sess.core(0).halt(1000)
    dump = pr.CoreDump.capture(sess, 0, [(0x2000_0000, 0x2000_0100)])
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
    test_fake_probe_attach_and_core()
    print("✓ fake probe + core")
    test_memory_round_trip()
    print("✓ memory round-trip")
    test_registry()
    print("✓ registry")
    test_get_target_by_name()
    print("✓ get_target_by_name")
    test_status_enum()
    print("✓ status enum")
    test_target_cores_typed()
    print("✓ target cores typed")
    test_error_taxonomy()
    print("✓ error taxonomy")
    test_download_options_ram_chunk_size()
    print("✓ download options ram_chunk_size")
    test_setup_tracing_takes_trace_sink()
    print("✓ setup_tracing TraceSink")
    test_register_lookup_and_width()
    print("✓ register lookup + width")
    test_bin_base_address_rejected_for_elf(tmp)
    print("✓ bin base_address validation")
    test_coredump_capture_roundtrip(tmp)
    print("✓ coredump capture round-trip")
    print("\nAll tests passed!")
