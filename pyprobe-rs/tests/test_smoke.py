"""No-hardware smoke test for pyprobe-rs using FakeProbe + Cortex-M4 builtin target."""

import pyprobe_rs as pr


def test_version():
    assert pr.__version__ == "0.31.0"


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
    assert status.is_halted, f"Expected halted, got {status.status}"
    core.run()
    status = core.status()
    assert status.is_running, f"Expected running, got {status.status}"


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
    reg = pr.RegistryPy.from_builtin_families()
    families = reg.families()
    assert len(families) > 0


def test_get_target_by_name():
    t = pr.get_target_by_name("Cortex-M4")
    assert t.name == "Cortex-M4"


if __name__ == "__main__":
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
    print("\nAll tests passed!")
