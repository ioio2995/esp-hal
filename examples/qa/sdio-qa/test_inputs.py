"""Software-only tests of the input validation: templates, unreviewed builds and missing
bootloader consent are refused."""
import copy
import hashlib
import importlib.util
from pathlib import Path

import pytest

HERE = Path(__file__).parent


def load(name):
    spec = importlib.util.spec_from_file_location("ti_" + name, HERE / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


inputs, fake, layout = load("inputs"), load("fake_image"), load("image_layout")
SHA = "a" * 40


@pytest.fixture
def case(tmp_path):
    artifacts = []
    for scenario in (*inputs.SCENARIOS, inputs.CONTROL_SCENARIO):
        for role, chip in (("host", "esp32p4"), ("slave", "esp32c6")):
            if scenario == inputs.CONTROL_SCENARIO and role == "slave":
                continue
            image = tmp_path / scenario / f"{role}.bin"
            image.parent.mkdir(exist_ok=True)
            image.write_bytes(fake.make(chip, app_bytes=8192 if scenario != "h1" else 4096))
            artifacts.append({"scenario": scenario, "role": role, "chip": chip,
                              "image": f"{scenario}/{role}.bin", "elf_sha256": "0" * 64,
                              "image_kind": "full-flash-prefix",
                              "image_sha256": hashlib.sha256(image.read_bytes()).hexdigest(),
                              "image_size": image.stat().st_size,
                              "layout": layout.parse(image.read_bytes(), chip),
                              "esp_hal": {"manifest_path": "/w/esp-hal/Cargo.toml", "registry_source": None}})
    build = {"source_sha": SHA, "production_driver_unchanged_from_base": True, "artifacts": artifacts}
    params = {"expected_source_sha": SHA,
              "p4": {"device": "board-p4", "chip": "esp32p4", "flash_size": 1 << 25, "transports": ["external_uart"]},
              "c6": {"device": "board-c6", "chip": "esp32c6", "flash_size": 1 << 23, "transports": ["external_uart"]}}
    consent = {"commit": SHA, "boards": ["board-p4", "board-c6"], "scenarios": ["h1", "h2", "h567"],
               "effects": "full flash-prefix image writes", "write_bootloader_and_partition_table": True,
               "operator_statement": "approved by the operator"}
    return params, consent, build, tmp_path


def test_valid_inputs_return_scenarios_and_regions_from_the_images(case):
    params, consent, build, directory = case
    scenarios, regions = inputs.validate(params, consent, build, directory)
    assert scenarios == ["h1", "h2", "h567"]
    assert regions["c6"][0] == 0 and regions["c6"][1] % 4096 == 0
    assert regions["p4"][1] >= 0x10000 + 8192  # the largest scenario image sets the region


@pytest.mark.parametrize("mutate,message", [
    (lambda p, c, b: c.update(commit="b" * 40), "same reviewed commit"),
    (lambda p, c, b: b.update(production_driver_unchanged_from_base=False), "production driver"),
    (lambda p, c, b: p["p4"].update(device="<logical id>"), "placeholder"),
    (lambda p, c, b: p["c6"].update(flash_size=0), "verified identify"),
    (lambda p, c, b: c.update(operator_statement="<the operator's own words>"), "written by the operator"),
    (lambda p, c, b: c.update(boards=["board-p4"]), "exactly the two boards"),
    (lambda p, c, b: c.update(scenarios=["h9"]), "known scenarios"),
    (lambda p, c, b: p["c6"].update(device="board-p4"), "distinct"),
    (lambda p, c, b: b["artifacts"][0]["esp_hal"].update(registry_source="registry+https://x"), "local esp-hal"),
    (lambda p, c, b: c.update(write_bootloader_and_partition_table=False), "bootloader"),
    (lambda p, c, b: c.pop("write_bootloader_and_partition_table"), "bootloader"),
    (lambda p, c, b: b["artifacts"][0].update(image_kind="app-only"), "full flash-prefix"),
    (lambda p, c, b: p["c6"].update(flash_size=4096), "out of bounds"),
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


def test_layout_differing_from_the_manifest_is_refused(case):
    params, consent, build, directory = case
    build["artifacts"][0]["layout"]["app"]["size"] += 4096
    with pytest.raises(inputs.InputError, match="layout differs"):
        inputs.validate(params, consent, build, directory)


def test_grant_must_cover_a_restoration(case):
    params, consent, build, directory = case
    params["grant_seconds"] = 900
    inputs.validate(params, consent, build, directory)
    params["grant_seconds"] = inputs.budget(inputs.RESTORE_TIMEOUT) - 1
    with pytest.raises(inputs.InputError, match="grant_seconds"):
        inputs.validate(params, consent, build, directory)
    assert inputs.budget(600) == 1290  # the Orchestrator default that 900 s cannot satisfy


def test_existing_valid_table_needs_explicit_acceptance():
    inputs.check_existing_table(None, {})  # blank or erased flash: nothing to accept
    table = [{"address": 0x9000, "type": 1, "size": 0x6000}]
    with pytest.raises(inputs.InputError, match="allow_overwrite_valid_table"):
        inputs.check_existing_table(table, {})
    inputs.check_existing_table(table, {"allow_overwrite_valid_table": True})


def test_written_table_must_equal_the_image_table(case):
    _, _, build, _ = case
    row = build["artifacts"][0]
    inputs.check_written_table([{k: p[k] for k in ("address", "type", "size")} for p in row["layout"]["partitions"]],
                               row["layout"])
    with pytest.raises(inputs.InputError, match="differs from the image"):
        inputs.check_written_table([], row["layout"])


def test_historical_backup_divergence_blocks():
    data = bytes(range(256)) * 32
    declared = {"sha256": hashlib.sha256(data).hexdigest()}
    inputs.check_historical(declared, data, data[0:4096], 0, 4096)
    with pytest.raises(inputs.InputError, match="differs from the fresh"):
        inputs.check_historical(declared, data, b"\0" * 4096, 0, 4096)
    with pytest.raises(inputs.InputError, match="declared SHA-256"):
        inputs.check_historical({"sha256": "0" * 64}, data, data[0:4096], 0, 4096)


def test_control_image_is_required(case):
    params, consent, build, directory = case
    build["artifacts"] = [a for a in build["artifacts"] if a["scenario"] != inputs.CONTROL_SCENARIO]
    with pytest.raises(inputs.InputError, match="c6ctl"):
        inputs.validate(params, consent, build, directory)


def test_control_image_is_not_a_consented_scenario(case):
    params, consent, build, directory = case
    consent["scenarios"] = ["c6ctl"]
    with pytest.raises(inputs.InputError, match="known scenarios"):
        inputs.validate(params, consent, build, directory)
