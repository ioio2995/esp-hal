"""Layout of a full flash-prefix image (bootloader, partition table, application).

`build.py` produces these images with `espflash save-image --merge --skip-padding`. The image
starts at flash address 0. Pure functions, no hardware: the layout read here is what the recipe
expects to find on the board after programming.
"""
import struct

CHIPS = {
    # chip: (ROM image chip id, second-stage bootloader offset)
    "esp32c6": (13, 0x0),
    "esp32p4": (18, 0x2000),
}
TABLE_OFFSET = 0x8000
SECTOR = 4096
MAGIC = 0xE9


class LayoutError(ValueError):
    pass


def _image_header(data, offset, chip_id, what):
    head = data[offset:offset + 24]
    if len(head) < 24 or head[0] != MAGIC:
        raise LayoutError(f"{what}: no ESP image header at {offset:#x}")
    found = struct.unpack("<H", head[12:14])[0]
    if found != chip_id:
        raise LayoutError(f"{what}: image is for chip id {found}, expected {chip_id}")
    return {"segments": head[1], "min_chip_rev_full": struct.unpack("<H", head[15:17])[0]}


def parse(image, chip):
    """Returns the layout, or raises `LayoutError` if the image is not a coherent full image."""
    if chip not in CHIPS:
        raise LayoutError(f"unsupported chip {chip}")
    chip_id, boot = CHIPS[chip]
    bootloader = _image_header(image, boot, chip_id, "bootloader")
    partitions, offset = [], TABLE_OFFSET
    while offset + 32 <= len(image):
        entry = image[offset:offset + 32]
        if entry[:2] == b"\xeb\xeb" or entry[:2] == b"\xff\xff":
            break
        if entry[:2] != b"\xaa\x50":
            raise LayoutError("invalid partition table entry magic")
        address, size = struct.unpack("<II", entry[4:12])
        partitions.append({"label": entry[12:28].split(b"\0")[0].decode(), "type": entry[2],
                           "subtype": entry[3], "address": address, "size": size})
        offset += 32
    apps = [p for p in partitions if p["type"] == 0]
    if len(apps) != 1:
        raise LayoutError("expected exactly one application partition")
    app = apps[0]
    app_header = _image_header(image, app["address"], chip_id, "application")
    for p in partitions:
        for q in partitions:
            if p is not q and p["address"] < q["address"] + q["size"] and q["address"] < p["address"] + p["size"]:
                raise LayoutError("overlapping partitions")
    if boot + 24 > TABLE_OFFSET or app["address"] < TABLE_OFFSET + SECTOR:
        raise LayoutError("bootloader, table and application are not in order")
    if len(image) > app["address"] + app["size"]:
        raise LayoutError("image is larger than its application partition")
    return {"chip": chip, "bootloader_offset": boot, "table_offset": TABLE_OFFSET,
            "partitions": partitions, "app": app, "write_address": 0, "image_size": len(image),
            "write_size": -(-len(image) // SECTOR) * SECTOR,
            "bootloader_min_chip_rev_full": bootloader["min_chip_rev_full"],
            "app_min_chip_rev_full": app_header["min_chip_rev_full"]}


def same_table(observed, layout):
    """The table read back from the board equals the one in the image (address, type, size)."""
    key = lambda rows: [(p["address"], p["type"], p["size"]) for p in rows]  # noqa: E731
    return key(observed) == key(layout["partitions"])
