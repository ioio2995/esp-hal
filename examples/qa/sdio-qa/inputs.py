"""Validation of the private parameters, the operator consent and the build manifest.

Pure functions, no network and no hardware. A template or an unreviewed build is refused here,
before any reservation is made.

The campaign writes FULL flash-prefix images (bootloader, partition table, application) at
flash address 0. The written region of a board is therefore computed from its images, never
declared by hand, and the consent must name the bootloader and partition-table write.
"""
import hashlib
import importlib.util
import json
from pathlib import Path

SCENARIOS = ("h1", "h2", "h3", "h4", "h567")
BOARD_KEYS = {"device", "chip", "flash_size", "transports"}
PROGRAM_TIMEOUT = 120
RESTORE_TIMEOUT = 300
MAX_REGION = 4 * 1024 * 1024


class InputError(ValueError):
    pass


def _layout():
    spec = importlib.util.spec_from_file_location("sdio_qa_image_layout",
                                                  Path(__file__).with_name("image_layout.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def budget(timeout):
    """Seconds of grant and reservation `Orchestrator.program()` demands for one operation:
    a write, an independent readback, two start windows and a margin."""
    return 2 * timeout + 90


def check_existing_table(table, consent):
    """`table` is the observed table, or None if none could be read (blank or erased flash).

    A valid existing table is never overwritten silently: the operator must have accepted it.
    """
    if table and consent.get("allow_overwrite_valid_table") is not True:
        raise InputError("a valid partition table exists on the board; the consent must set "
                         "allow_overwrite_valid_table after the operator reviewed it")


def check_written_table(observed, layout):
    """After programming, the table read from the board must be the one of the image."""
    if not _layout().same_table(observed, layout):
        raise InputError("the partition table read back differs from the image's table")


def check_historical(declared, data, fresh, address, size):
    """A configured historical backup is a prerequisite: it must match its digest and the
    freshly read region, otherwise programming is blocked."""
    if hashlib.sha256(data).hexdigest() != declared["sha256"]:
        raise InputError("historical backup does not match its declared SHA-256")
    if data[address:address + size] != fresh:
        raise InputError("historical backup differs from the fresh read of the region")


def validate(params, consent, build_manifest, build_dir):
    """Returns `(scenarios, regions)` where `regions[role]` is `(address, size)` per board key.

    Raises `InputError` naming the first problem.
    """
    for role, chip in (("p4", "esp32p4"), ("c6", "esp32c6")):
        board = params.get(role)
        if not isinstance(board, dict) or not BOARD_KEYS <= set(board):
            raise InputError(f"parameters.{role} is incomplete")
        if board["chip"] != chip:
            raise InputError(f"parameters.{role}.chip must be {chip}")
        if not isinstance(board["device"], str) or board["device"].startswith("<"):
            raise InputError(f"parameters.{role}.device is still a placeholder")
        if type(board["flash_size"]) is not int or board["flash_size"] <= 0:
            raise InputError(f"parameters.{role}.flash_size must come from a verified identify")
    grant = params.get("grant_seconds", 900)
    if type(grant) is not int or not budget(RESTORE_TIMEOUT) <= grant <= 14400:
        raise InputError(f"grant_seconds must cover a restoration: at least {budget(RESTORE_TIMEOUT)}")
    if params["p4"]["device"] == params["c6"]["device"]:
        raise InputError("the P4 and the C6 must be distinct logical boards")
    commit = params.get("expected_source_sha")
    if not commit or commit != consent.get("commit") or commit != build_manifest.get("source_sha"):
        raise InputError("parameters, consent and build manifest must name the same reviewed commit")
    if build_manifest.get("production_driver_unchanged_from_base") is not True:
        raise InputError("the build does not prove the production driver is unchanged")
    if set(consent.get("boards", [])) != {params["p4"]["device"], params["c6"]["device"]}:
        raise InputError("consent must name exactly the two boards of the parameters")
    if consent.get("write_bootloader_and_partition_table") is not True:
        raise InputError("consent must explicitly authorize writing the bootloader and the "
                         "partition table (write_bootloader_and_partition_table)")
    scenarios = consent.get("scenarios")
    if not scenarios or not set(scenarios) <= set(SCENARIOS):
        raise InputError("consent must list known scenarios")
    for key in ("effects", "operator_statement"):
        if not isinstance(consent.get(key), str) or not consent[key].strip() or consent[key].startswith("<"):
            raise InputError(f"consent.{key} must be written by the operator")
    layout = _layout()
    build_dir = Path(build_dir)
    sizes = {"p4": 0, "c6": 0}
    for scenario in scenarios:
        for role, key in (("host", "p4"), ("slave", "c6")):
            rows = [a for a in build_manifest["artifacts"] if a["scenario"] == scenario and a["role"] == role]
            if len(rows) != 1:
                raise InputError(f"build manifest lacks exactly one {role} artifact for {scenario}")
            row = rows[0]
            if row["chip"] != params[key]["chip"]:
                raise InputError(f"{scenario}/{role} was built for another chip")
            if row.get("image_kind") != "full-flash-prefix":
                raise InputError(f"{scenario}/{role} is not a full flash-prefix image")
            image = build_dir / row["image"]
            if sha256_file(image) != row["image_sha256"] or image.stat().st_size != row["image_size"]:
                raise InputError(f"{scenario}/{role} image differs from the build manifest")
            try:
                found = layout.parse(image.read_bytes(), row["chip"])
            except layout.LayoutError as error:
                raise InputError(f"{scenario}/{role}: {error}") from error
            if found != row["layout"]:
                raise InputError(f"{scenario}/{role} layout differs from the build manifest")
            if found["write_address"] != 0 or found["write_size"] > MAX_REGION \
                    or found["write_size"] > params[key]["flash_size"]:
                raise InputError(f"{scenario}/{role} write region is out of bounds")
            if row["esp_hal"]["registry_source"] is not None:
                raise InputError(f"{scenario}/{role} did not use the local esp-hal")
            sizes[key] = max(sizes[key], found["write_size"])
    return list(scenarios), {key: (0, size) for key, size in sizes.items()}


def load_json(path):
    return json.loads(Path(path).read_text())
