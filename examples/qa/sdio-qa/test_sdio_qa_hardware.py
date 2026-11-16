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
import importlib.util
import sys
from pathlib import Path

import pytest

pytest.importorskip("iobewi_bench")
from iobewi_bench.orchestration import Orchestrator  # noqa: E402
from iobewi_bench.qualification import Campaign  # noqa: E402

HERE = Path(__file__).parent


spec = importlib.util.spec_from_file_location("sdio_qa_campaign", HERE / "campaign.py")
campaign_flow = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign_flow)
inputs = campaign_flow.inputs

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
        campaigns[key].write("support-sha256.json", {n: inputs.sha256_file(HERE / (n + ".py")) for n in ("verdict", "inputs", "campaign")})
        orchestrators[key] = Orchestrator(campaigns[key])
    ttl = 1800
    try:
        for key in ("p4", "c6"):
            boards[key] = campaigns[key].reserve(ttl=ttl)
    except BaseException:
        for campaign in campaigns.values():
            campaign.close()
        raise
    outcome = campaign_flow.run(
        params, scenarios, build, build_dir, campaigns, orchestrators, boards,
        campaign_flow.Console, lambda label: _gesture(label, interactive), ttl=ttl)
    assert not outcome.cleanup_errors, f"restoration or cleanup incomplete: {outcome.cleanup_errors}"
    assert outcome.error is None, f"campaign stopped: {type(outcome.error).__name__}: {outcome.error}"
    assert not outcome.failed_verdicts, f"scenario verdicts: {outcome.failed_verdicts}"
