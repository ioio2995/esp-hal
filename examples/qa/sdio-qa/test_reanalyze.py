"""Software-only tests of the evidence reanalysis tool, on synthetic captures."""
import hashlib
import importlib.util
import json
from pathlib import Path

import pytest

HERE = Path(__file__).parent


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


tb = load("tr_tb", "test_bench_verdict.py")
reanalyze = load("tr_re", "reanalyze.py")
IDENTITY = {"commit": "c" * 40, "evaluator_files_modified": False, "files": {"verdict.py": "1", "bench_verdict.py": "2"}}


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


@pytest.fixture
def evidence(tmp_path):
    """A finished run whose original verdict was FAIL because of the C6 idle timeout."""
    lines = tb.add_after_last_phase(tb.make(tb.cfgs("clean"), confirm=False, measurements=1, plan="smoke"), tb.IDLE)
    for key, board in (("p4", "P4"), ("c6", "C6")):
        folder = tmp_path / ("sdio-aaaaaaaaaaaa-" + key)
        folder.mkdir()
        mine = [[x.board, x.time_ms, x.body] for x in lines if x.board == board]
        (folder / "uart-bench-smoke-lines.json").write_text(json.dumps({"complete_observation": True, "error": None, "lines": mine}))
        (folder / "uart-bench-smoke.log").write_bytes(b"raw " + key.encode())
    p4 = tmp_path / "sdio-aaaaaaaaaaaa-p4"
    (p4 / "run.json").write_text(json.dumps({"revisions": {"source_sha": "a" * 40}}))
    (p4 / "verdict-bench-smoke.json").write_text(json.dumps({
        "status": "FAIL", "checks": [{"check": "no clean configuration has a failing C6 session", "ok": False, "detail": [0]}],
        "report": None}))
    return p4, tmp_path / "sdio-aaaaaaaaaaaa-c6"


def test_reanalysis_records_firmware_evaluator_inputs_and_both_verdicts(evidence):
    p4, c6 = evidence
    record = reanalyze.reanalyze(p4, c6, "bench-smoke", identity=IDENTITY)
    assert record["executed_firmware_source_sha"] == "a" * 40
    assert record["evaluator"]["commit"] == "c" * 40
    assert record["original_verdict"]["status"] == "FAIL"
    assert record["original_verdict"]["failed_checks"] == ["no clean configuration has a failing C6 session"]
    assert record["reanalysed_verdict"]["status"] == "PASS"
    for key, folder in (("p4", p4), ("c6", c6)):
        assert record["inputs"][key]["lines_sha256"] == digest(folder / "uart-bench-smoke-lines.json")
        assert record["inputs"][key]["raw_log_sha256"] == digest(folder / "uart-bench-smoke.log")
    assert "does not turn it into a pass" in record["note"]


def test_the_original_evidence_is_never_modified(evidence):
    p4, c6 = evidence
    before = {str(p): digest(p) for folder in (p4, c6) for p in folder.iterdir()}
    reanalyze.write_new(p4, reanalyze.reanalyze(p4, c6, "bench-smoke", identity=IDENTITY))
    after = {str(p): digest(p) for folder in (p4, c6) for p in folder.iterdir() if not p.name.startswith("reanalysis-")}
    assert after == before


def test_a_reanalysis_is_never_overwritten(evidence):
    p4, c6 = evidence
    record = reanalyze.reanalyze(p4, c6, "bench-smoke", identity=IDENTITY)
    target = reanalyze.write_new(p4, record)
    assert target.name == "reanalysis-bench-smoke-" + "c" * 12 + ".json"
    with pytest.raises(FileExistsError):
        reanalyze.write_new(p4, record)


def test_a_real_failure_in_the_captures_stays_a_failure_in_the_reanalysis(evidence):
    p4, c6 = evidence
    saved = json.loads((c6 / "uart-bench-smoke-lines.json").read_text())
    saved["lines"] = [[b, t, x.replace("receive timed out", "content mismatch")] for b, t, x in saved["lines"]]
    (c6 / "uart-bench-smoke-lines.json").write_text(json.dumps(saved))
    assert reanalyze.reanalyze(p4, c6, "bench-smoke", identity=IDENTITY)["reanalysed_verdict"]["status"] == "FAIL"
