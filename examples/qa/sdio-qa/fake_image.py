"""Synthetic full flash-prefix images for the software tests. Not firmware."""
import struct

IDS = {"esp32c6": (13, 0x0), "esp32p4": (18, 0x2000)}


def header(chip_id, filler=b"\0"):
    return bytes([0xE9, 3, 2, 0x30]) + b"\0" * 4 + b"\0" * 4 + struct.pack("<H", chip_id) + b"\0" \
        + struct.pack("<H", 0) + struct.pack("<H", 0xFFFF) + b"\0" * 8


def entry(label, ptype, subtype, address, size):
    return b"\xaa\x50" + bytes([ptype, subtype]) + struct.pack("<II", address, size) \
        + label.encode().ljust(16, b"\0") + b"\0" * 4


def make(chip, app_bytes=4096, table=None):
    chip_id, boot = IDS[chip]
    table = table or [("nvs", 1, 2, 0x9000, 0x6000), ("phy_init", 1, 1, 0xF000, 0x1000),
                      ("factory", 0, 0, 0x10000, 0x100000)]
    data = bytearray(b"\xff" * (0x10000 + app_bytes))
    data[boot:boot + 24] = header(chip_id)
    for i, row in enumerate(table):
        data[0x8000 + 32 * i:0x8000 + 32 * i + 32] = entry(*row)
    data[0x8000 + 32 * len(table):0x8000 + 32 * len(table) + 2] = b"\xeb\xeb"
    data[0x10000:0x10000 + 24] = header(chip_id)
    return bytes(data)
