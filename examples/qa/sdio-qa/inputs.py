"""Validation of the private parameters, the operator consent and the build manifest.

Pure functions, no network and no hardware. A template or an unreviewed build is refused here,
before any reservation is made.
"""
import hashlib
import json
from pathlib import Path

SCENARIOS = ("h1", "h2", "h3", "h4", "h567")
BOARD_KEYS = {"device", "chip", "flash_size", "transports", "app_offset", "app_size"}


class InputError(ValueError):
    pass


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _sectors(value):
    return type(value) is int and value > 0 and value % 4096 == 0


def validate(params, consent, build_manifest, build_dir):
    """Returns the scenarios to run, or raises `InputError` naming the first problem."""
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
        if type(board["app_offset"]) is not int or board["app_offset"] % 4096 or board["app_offset"] <= 0:
            raise InputError(f"parameters.{role}.app_offset must be a sector-aligned app partition")
        if not _sectors(board["app_size"]) or board["app_offset"] + board["app_size"] > board["flash_size"]:
            raise InputError(f"parameters.{role}.app_size must be whole sectors inside the flash")
    if params["p4"]["device"] == params["c6"]["device"]:
        raise InputError("the P4 and the C6 must be distinct logical boards")
    commit = params.get("expected_source_sha")
    if not commit or commit != consent.get("commit") or commit != build_manifest.get("source_sha"):
        raise InputError("parameters, consent and build manifest must name the same reviewed commit")
    if build_manifest.get("production_driver_unchanged_from_base") is not True:
        raise InputError("the build does not prove the production driver is unchanged")
    if set(consent.get("boards", [])) != {params["p4"]["device"], params["c6"]["device"]}:
        raise InputError("consent must name exactly the two boards of the parameters")
    scenarios = consent.get("scenarios")
    if not scenarios or not set(scenarios) <= set(SCENARIOS):
        raise InputError("consent must list known scenarios")
    for key in ("effects", "operator_statement"):
        if not isinstance(consent.get(key), str) or not consent[key].strip() or consent[key].startswith("<"):
            raise InputError(f"consent.{key} must be written by the operator")
    build_dir = Path(build_dir)
    for scenario in scenarios:
        for role, key in (("host", "p4"), ("slave", "c6")):
            rows = [a for a in build_manifest["artifacts"] if a["scenario"] == scenario and a["role"] == role]
            if len(rows) != 1:
                raise InputError(f"build manifest lacks exactly one {role} artifact for {scenario}")
            row = rows[0]
            if row["chip"] != params[key]["chip"]:
                raise InputError(f"{scenario}/{role} was built for another chip")
            image = build_dir / row["image"]
            if sha256_file(image) != row["image_sha256"] or image.stat().st_size != row["image_size"]:
                raise InputError(f"{scenario}/{role} image differs from the build manifest")
            if row["image_size"] > params[key]["app_size"]:
                raise InputError(f"{scenario}/{role} image does not fit the app partition")
            if row["esp_hal"]["registry_source"] is not None:
                raise InputError(f"{scenario}/{role} did not use the local esp-hal")
    return list(scenarios)


def load_json(path):
    return json.loads(Path(path).read_text())
