"""H0-H7 hardware qualification of the native SDIO driver on an ESP32-P4 / ESP32-C6 pair.

This recipe changes real flash. It is skipped unless ALL of these hold:

* `--bench`, `--bench-allow-destructive`, `BENCH_URL` and `BENCH_CLIENT_TOKEN` (never in argv);
* `--sdio-qa-params` and `--sdio-qa-consent`, written outside Git, naming the reviewed commit;
* a server grant and a local operator, for every programming operation.

Collection alone never reaches the network. A pytest opt-in is not a consent: see SDIO-QA.md.
Run with `-s --sdio-qa-interactive` so the operator is prompted for every physical action.

Contract per phase (details in SDIO-QA.md):

* H0: identify both boards, read the app partition twice, program only the app partition of each
  board with an independent readback, never erase, never touch bootloader or partition table.
* H1-H7: observe the two consoles and evaluate them with `verdict.py`.
* Closing: restore both app partitions, compare, release reservations, local grant revoked.

No operation is retried. A failure stops the campaign at the first failed step and restoration
still runs; an unknown outcome is recorded as INDETERMINATE.
"""
import hashlib
import importlib.util
import json
import sys
import time
from pathlib import Path

import pytest

pytest.importorskip("iobewi_bench")
from iobewi_bench.client import BenchError  # noqa: E402
from iobewi_bench.orchestration import Orchestrator  # noqa: E402
from iobewi_bench.qualification import Campaign  # noqa: E402

HERE = Path(__file__).parent


def _module(name):
    spec = importlib.util.spec_from_file_location("sdio_qa_" + name, HERE / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verdict = _module("verdict")
inputs = _module("inputs")

pytestmark = pytest.mark.bench


def _manifest(params, scenarios, evidence_root):
    boards, regions = [], []
    for key in ("p4", "c6"):
        b = params[key]
        boards.append({"id": b["device"], "chip": b["chip"], "flash_size": b["flash_size"],
                       "transports": b["transports"]})
        regions.append({"board": b["device"], "address": b["app_offset"], "size": b["app_size"]})
    return {
        "version": 1, "id": "sdio-p4-c6-" + params["expected_source_sha"][:12],
        "boards": boards, "write_regions": regions, "max_interruptions": 0,
        "backup_policy": "Two sequential reads of each app partition must be identical before any write.",
        "restore_policy": "Rewrite the verified app-partition backup, then compare it by independent readback.",
        "grant_seconds": params.get("grant_seconds", 900),
        "operator_steps": [
            "Put the C6 in ROM download without resetting it through GPIO54 (P4 held in reset).",
            "Put the P4 in download mode, or use its UART bridge, as declared in the parameters.",
            "Reset the P4 so that its firmware boots the scenario; the P4 firmware resets the C6.",
            "Grant local control for each programming operation, then revoke it.",
        ],
        "start_criteria": ["Both boards connected, unreserved, not quarantined, build manifest verified."],
        "close_criteria": ["App partitions restored and compared, reservations released, grants revoked."],
        "private_evidence": str(evidence_root),
    }


class Console:
    """One board's UART events assembled into QA lines for the current scenario."""

    def __init__(self, board):
        self.board, self.cursor = board, None
        self.assembler = verdict.LineAssembler()
        self.raw = bytearray()

    def start(self):
        self.cursor = self.board.cursor()

    def poll(self):
        import base64
        for event in self.board.events(self.cursor):
            if event["kind"] == "uart":
                data = base64.b64decode(event["data"], validate=True)
                self.raw += data
                self.assembler.feed(data)


def _gesture(label, interactive):
    print("\nOPERATOR ACTION: " + label, flush=True)
    if interactive:
        sys.stdin.readline()


def test_sdio_qualification(bench_client, bench_run_dir, pytestconfig):
    if not pytestconfig.getoption("--bench-allow-destructive"):
        pytest.skip("Programming requires --bench-allow-destructive")
    params_path = pytestconfig.getoption("--sdio-qa-params")
    consent_path = pytestconfig.getoption("--sdio-qa-consent")
    if not params_path or not consent_path:
        pytest.skip("Requires --sdio-qa-params and --sdio-qa-consent (private files)")
    interactive = pytestconfig.getoption("--sdio-qa-interactive")
    params, consent = inputs.load_json(params_path), inputs.load_json(consent_path)
    build_dir = Path(params["build_dir"])
    build = inputs.load_json(build_dir / "build-manifest.json")
    scenarios = inputs.validate(params, consent, build, build_dir)

    root = bench_run_dir / ("sdio-" + params["expected_source_sha"][:12])
    root.mkdir(mode=0o700)
    manifest = _manifest(params, scenarios, Path(params["evidence_root"]).resolve())
    campaigns, boards, orchestrators = {}, {}, {}
    for key in ("p4", "c6"):
        device = params[key]["device"]
        campaigns[key] = Campaign(bench_client, device, Path(params["evidence_root"]) / (root.name + "-" + key),
                                  Path(__file__), "sdio-" + key, allow_destructive=True,
                                  manifest=manifest, revisions={"source_sha": params["expected_source_sha"],
                                  "build_manifest_sha256": inputs.sha256_file(build_dir / "build-manifest.json")},
                                  parameters=params)
        campaigns[key].start()
        campaigns[key].write("support-sha256.json", {n: inputs.sha256_file(HERE / (n + ".py")) for n in ("verdict", "inputs")})
        orchestrators[key] = Orchestrator(campaigns[key])
    restored, backups, results = {}, {}, {}
    try:
        ttl = 1800
        for key in ("p4", "c6"):
            boards[key] = campaigns[key].reserve(ttl=ttl)
        # H0 identification and backup. Two identical reads are required before any write.
        for key in ("p4", "c6"):
            board, spec = boards[key], params[key]
            identity = board.identify()
            campaigns[key].write("h0-identify.json", {k: identity.get(k) for k in ("chip", "flash_size")})
            assert identity["chip"] == spec["chip"] and identity["flash_size"] == spec["flash_size"]
            table = board.partitions()
            campaigns[key].write("h0-partitions.json", table)
            assert any(int(p.get("offset", -1)) == spec["app_offset"] for p in table), \
                "app_offset is not a partition of the observed table"
            a, meta_a = orchestrators[key].backup(board, address=spec["app_offset"], size=spec["app_size"],
                                                  filename="backup-a.bin")
            b, meta_b = orchestrators[key].backup(board, address=spec["app_offset"], size=spec["app_size"],
                                                  filename="backup-b.bin")
            assert meta_a["sha256"] == meta_b["sha256"], "two reads of the app partition differ: no write allowed"
            backups[key] = a
        history = params["p4"].get("historical_backup")
        if history:
            data = Path(history["path"]).read_bytes()
            slice_ = data[params["p4"]["app_offset"]:params["p4"]["app_offset"] + params["p4"]["app_size"]]
            campaigns["p4"].write("h0-historical-crosscheck.json", {
                "historical_file_sha256": hashlib.sha256(data).hexdigest(),
                "declared_sha256": history["sha256"],
                "slice_equals_fresh_backup": slice_ == backups["p4"].read_bytes()})

        for scenario in scenarios:
            rows = {r["role"]: r for r in build["artifacts"] if r["scenario"] == scenario}
            for key, role in (("c6", "slave"), ("p4", "host")):
                row = rows[role]
                _gesture(f"{scenario}: put the {key.upper()} in loader mode as declared; "
                         f"hold the P4 in reset while the C6 is loaded", interactive)
                orchestrators[key].wait_ready(timeout=300, grant_seconds=params.get("grant_seconds", 900))
                orchestrators[key].program(boards[key], address=params[key]["app_offset"],
                                           source=build_dir / row["image"], sha256=row["image_sha256"],
                                           allow_destructive=True)
                campaigns[key].write(f"h0-program-{scenario}.json", {
                    "image_sha256": row["image_sha256"], "elf_sha256": row["elf_sha256"],
                    "address": params[key]["app_offset"], "readback": "independent flash.read equal"})
            consoles = {key: Console(boards[key]) for key in ("p4", "c6")}
            for key, console in consoles.items():
                boards[key].console()
                console.start()
            _gesture(f"{scenario}: release the P4 reset / reset the P4 so it boots; do not reset the C6", interactive)
            deadline = time.monotonic() + params.get("scenario_timeout_seconds", 90)
            while time.monotonic() < deadline:
                for console in consoles.values():
                    console.poll()
                lines = consoles["p4"].assembler.lines + consoles["c6"].assembler.lines
                if any(x.kind == "RESULT" for x in consoles["p4"].assembler.lines) and any(
                        x.kind == "RESULT" for x in consoles["c6"].assembler.lines):
                    break
                time.sleep(0.2)
            lines = consoles["p4"].assembler.lines + consoles["c6"].assembler.lines
            for key, console in consoles.items():
                (campaigns[key].directory / f"uart-{scenario}.log").write_bytes(bytes(console.raw))
            for test_id, outcome in verdict.evaluate(scenario, lines).items():
                results[test_id] = outcome
                campaigns["p4"].result(test_id, outcome.status, preconditions=f"build {params['expected_source_sha']}",
                                       evidence=json.dumps(outcome.checks)[:2000], incidents="",
                                       restoration="pending", limits="app partitions only")
        failed = {k: v.status for k, v in results.items() if v.status != "PASS"}
        assert not failed, f"scenario verdicts: {failed}"
    finally:
        errors = []
        for key in ("c6", "p4"):
            try:
                if key in backups:
                    _gesture(f"restore {key.upper()}: put it in loader mode again", interactive)
                    orchestrators[key].wait_ready(timeout=300, grant_seconds=params.get("grant_seconds", 900))
                    orchestrators[key].restore(boards[key], backups[key], allow_destructive=True)
                    restored[key] = orchestrators[key].compare(boards[key], backups[key])
                    campaigns[key].write("restoration.json", restored[key])
            except (BenchError, AssertionError, TimeoutError, OSError) as error:
                errors.append((key, type(error).__name__))
        for campaign in campaigns.values():
            try:
                campaign.close()
            except BaseException as error:  # noqa: BLE001 - reported with the restoration outcome
                errors.append(("close", type(error).__name__))
        if errors:
            pytest.fail(f"restoration or cleanup incomplete: {errors}", pytrace=False)
