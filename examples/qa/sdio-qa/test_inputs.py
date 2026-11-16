"""Software-only tests of the input validation: templates and mismatches are refused."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path

import pytest

spec = importlib.util.spec_from_file_location("sdio_qa_inputs", Path(__file__).with_name("inputs.py"))
inputs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inputs)

SHA = "a" * 40


@pytest.fixture
def case(tmp_path):
    artifacts = []
    for scenario in inputs.SCENARIOS:
        for role, chip in (("host", "esp32p4"), ("slave", "esp32c6")):
            image = tmp_path / scenario / f"{role}.bin"
            image.parent.mkdir(exist_ok=True)
            image.write_bytes(b"image-" + role.encode() + scenario.encode())
            artifacts.append({"scenario": scenario, "role": role, "chip": chip,
                              "image": f"{scenario}/{role}.bin", "elf_sha256": "0" * 64,
                              "image_sha256": hashlib.sha256(image.read_bytes()).hexdigest(),
                              "image_size": image.stat().st_size,
                              "esp_hal": {"manifest_path": "/w/esp-hal/Cargo.toml", "registry_source": None}})
    build = {"source_sha": SHA, "production_driver_unchanged_from_base": True, "artifacts": artifacts}
    params = {"expected_source_sha": SHA,
              "p4": {"device": "board-p4", "chip": "esp32p4", "flash_size": 1 << 25,
                     "transports": ["external_uart"], "app_offset": 0x110000, "app_size": 0x100000},
              "c6": {"device": "board-c6", "chip": "esp32c6", "flash_size": 1 << 22,
                     "transports": ["external_uart"], "app_offset": 0x10000, "app_size": 0x100000}}
    consent = {"commit": SHA, "boards": ["board-p4", "board-c6"], "scenarios": ["h1", "h567"],
               "effects": "app partition writes", "operator_statement": "approved by the operator"}
    return params, consent, build, tmp_path


def test_valid_inputs_return_the_consented_scenarios(case):
    params, consent, build, directory = case
    assert inputs.validate(params, consent, build, directory) == ["h1", "h567"]


@pytest.mark.parametrize("mutate,message", [
    (lambda p, c, b: c.update(commit="b" * 40), "same reviewed commit"),
    (lambda p, c, b: b.update(production_driver_unchanged_from_base=False), "production driver"),
    (lambda p, c, b: p["p4"].update(device="<logical id>"), "placeholder"),
    (lambda p, c, b: p["c6"].update(flash_size=0), "verified identify"),
    (lambda p, c, b: p["c6"].update(app_offset=0), "sector-aligned"),
    (lambda p, c, b: c.update(operator_statement="<the operator's own words>"), "written by the operator"),
    (lambda p, c, b: c.update(boards=["board-p4"]), "exactly the two boards"),
    (lambda p, c, b: c.update(scenarios=["h9"]), "known scenarios"),
    (lambda p, c, b: p["c6"].update(device="board-p4"), "distinct"),
    (lambda p, c, b: b["artifacts"][0]["esp_hal"].update(registry_source="registry+https://x"), "local esp-hal"),
])
def test_refusals(case, mutate, message):
    params, consent, build, directory = (copy.deepcopy(x) if not isinstance(x, Path) else x for x in case)
    mutate(params, consent, build)
    with pytest.raises(inputs.InputError, match=message):
        inputs.validate(params, consent, build, directory)


def test_modified_image_is_refused(case):
    params, consent, build, directory = case
    (directory / "h1" / "host.bin").write_bytes(b"tampered")
    with pytest.raises(inputs.InputError, match="differs from the build manifest"):
        inputs.validate(params, consent, build, directory)


def test_image_larger_than_partition_is_refused(case):
    params, consent, build, directory = case
    params["c6"]["app_size"] = 4096
    big = directory / "h1" / "slave.bin"
    big.write_bytes(b"x" * 8192)
    row = next(a for a in build["artifacts"] if a["scenario"] == "h1" and a["role"] == "slave")
    row["image_sha256"], row["image_size"] = hashlib.sha256(big.read_bytes()).hexdigest(), 8192
    with pytest.raises(inputs.InputError, match="does not fit"):
        inputs.validate(params, consent, build, directory)


TABLE = [
    {"label": "nvs", "type": 1, "subtype": 2, "address": 0x9000, "size": 0x6000},
    {"label": "factory", "type": 0, "subtype": 0, "address": 0x110000, "size": 0x100000},
    {"label": "storage", "type": 1, "subtype": 1, "address": 0x210000, "size": 0x100000},
]
SPEC = {"app_offset": 0x110000, "app_size": 0x100000}


def test_exact_app_partition_is_accepted():
    assert inputs.check_partition(TABLE, SPEC)["label"] == "factory"


@pytest.mark.parametrize("spec,table,message", [
    ({"app_offset": 0x210000, "app_size": 0x100000}, TABLE, "not an application"),
    ({"app_offset": 0x110000, "app_size": 0x200000}, TABLE, "differs from the observed"),
    ({"app_offset": 0x111000, "app_size": 0x1000}, TABLE, "no single partition"),
    (SPEC, TABLE[:1] + [{"label": "f", "type": 0, "subtype": 0, "address": 0x110000, "size": 0x100000},
                        {"label": "o", "type": 1, "subtype": 1, "address": 0x1ff000, "size": 0x2000}],
     "overlaps"),
])
def test_wrong_partition_or_overflow_is_refused(spec, table, message):
    with pytest.raises(inputs.InputError, match=message):
        inputs.check_partition(table, spec)


def test_grant_must_cover_a_restoration(case):
    params, consent, build, directory = case
    params["grant_seconds"] = 900
    inputs.validate(params, consent, build, directory)
    params["grant_seconds"] = inputs.budget(inputs.RESTORE_TIMEOUT) - 1
    with pytest.raises(inputs.InputError, match="grant_seconds"):
        inputs.validate(params, consent, build, directory)
    assert inputs.budget(600) == 1290  # the Orchestrator default that 900 s cannot satisfy


def test_historical_backup_divergence_blocks():
    data = bytes(range(256)) * 32
    declared = {"sha256": hashlib.sha256(data).hexdigest()}
    inputs.check_historical(declared, data, data[4096:8192], 4096, 4096)
    with pytest.raises(inputs.InputError, match="differs from the fresh"):
        inputs.check_historical(declared, data, b"\0" * 4096, 4096, 4096)
    with pytest.raises(inputs.InputError, match="declared SHA-256"):
        inputs.check_historical({"sha256": "0" * 64}, data, data[4096:8192], 4096, 4096)
