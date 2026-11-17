"""Software-only tests of the full-image layout parser, on synthetic images and, when present,
on nothing else: a PASS here is not a statement about espflash's real output."""
import importlib.util
from pathlib import Path

import pytest

HERE = Path(__file__).parent


def load(name):
    spec = importlib.util.spec_from_file_location("t_" + name, HERE / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


layout, fake = load("image_layout"), load("fake_image")


@pytest.mark.parametrize("chip,boot", [("esp32c6", 0), ("esp32p4", 0x2000)])
def test_valid_full_image(chip, boot):
    found = layout.parse(fake.make(chip), chip)
    assert found["bootloader_offset"] == boot and found["write_address"] == 0
    assert found["app"]["address"] == 0x10000 and found["write_size"] % 4096 == 0
    assert [p["label"] for p in found["partitions"]] == ["nvs", "phy_init", "factory"]


def test_image_for_the_wrong_chip_is_refused():
    with pytest.raises(layout.LayoutError, match="bootloader"):
        layout.parse(fake.make("esp32c6"), "esp32p4")
    data = bytearray(fake.make("esp32c6"))
    data[12:14] = (18).to_bytes(2, "little")  # a bootloader built for another chip
    with pytest.raises(layout.LayoutError, match="chip id"):
        layout.parse(bytes(data), "esp32c6")


def test_missing_bootloader_is_refused():
    data = bytearray(fake.make("esp32p4"))
    data[0x2000] = 0xFF
    with pytest.raises(layout.LayoutError, match="bootloader"):
        layout.parse(bytes(data), "esp32p4")


def test_overlapping_partitions_are_refused():
    table = [("nvs", 1, 2, 0x9000, 0x7000), ("phy_init", 1, 1, 0xF000, 0x1000), ("factory", 0, 0, 0x10000, 0x1000)]
    with pytest.raises(layout.LayoutError, match="overlapping"):
        layout.parse(fake.make("esp32c6", table=table), "esp32c6")


def test_image_larger_than_its_app_partition_is_refused():
    table = [("nvs", 1, 2, 0x9000, 0x6000), ("factory", 0, 0, 0x10000, 0x1000)]
    with pytest.raises(layout.LayoutError, match="larger"):
        layout.parse(fake.make("esp32c6", app_bytes=0x3000, table=table), "esp32c6")


def test_no_or_two_app_partitions_are_refused():
    with pytest.raises(layout.LayoutError, match="exactly one"):
        layout.parse(fake.make("esp32c6", table=[("nvs", 1, 2, 0x9000, 0x6000)]), "esp32c6")


def test_table_comparison():
    found = layout.parse(fake.make("esp32c6"), "esp32c6")
    assert layout.same_table(found["partitions"], found)
    assert not layout.same_table(found["partitions"][:2], found)
