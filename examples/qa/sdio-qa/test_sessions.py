"""Software-only tests of the P4 session handling: several boots in one capture are never merged
silently, and a reanalysis judges one session with the C6 sessions that belong to it."""
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


tb = load("ts_tb", "test_bench_verdict.py")
re_ = load("ts_re", "reanalyze.py")
verdict, bench = tb.verdict, tb.bench
IDENTITY = {"commit": "d" * 40, "evaluator_files_modified": False, "files": {}}
PLAN = dict(plan="qualified")


def split(lines):
    return [x for x in lines if x.board == "P4"], [x for x in lines if x.board == "C6"]


def run_lines(*kinds, **kw):
    return split(tb.make(tb.cfgs(*kinds), **PLAN, **kw))


def partial(*kinds):
    """The beginning of a plan: the P4 stopped after its first measurement window."""
    p4, c6 = run_lines(*kinds)
    cut = next(i for i, x in enumerate(p4) if x.kind == "bench_result") + 1
    first_session = []
    for x in c6[1:]:
        if x.kind == "boot":
            break
        first_session.append(x)
    return p4[:cut], [c6[0]] + first_session[:3]


def capture(*runs):
    """P4 lines of every run then C6 lines of every run, as one capture."""
    return [x for p4, _ in runs for x in p4] + [x for _, c6 in runs for x in c6]


def evaluate(lines):
    return verdict.evaluate("bench-qualified", lines)["bench-qualified"]


def test_two_complete_plans_are_not_merged_by_the_live_evaluation():
    result = evaluate(capture(run_lines("clean", "clean"), run_lines("clean", "clean")))
    assert result.status == "INDETERMINATE"
    assert any(not c["ok"] and "single P4 session" in c["check"] for c in result.checks)


def test_a_complete_plan_followed_by_the_start_of_another_is_not_judged_as_one_run():
    assert evaluate(capture(run_lines("clean", "clean"), partial("clean", "clean"))).status == "INDETERMINATE"


def folder(tmp_path, lines, name):
    for key, board in (("p4", "P4"), ("c6", "C6")):
        d = tmp_path / ("sdio-bbbbbbbbbbbb-" + key)
        d.mkdir(exist_ok=True)
        mine = [[x.board, x.time_ms, x.body] for x in lines if x.board == board]
        (d / "uart-bench-qualified-lines.json").write_text(json.dumps({"complete_observation": True, "error": None, "lines": mine}))
        (d / "uart-bench-qualified.log").write_bytes(b"raw " + key.encode())
    p4 = tmp_path / "sdio-bbbbbbbbbbbb-p4"
    (p4 / "run.json").write_text(json.dumps({"revisions": {"source_sha": "b" * 40}}))
    (p4 / "verdict-bench-qualified.json").write_text(json.dumps({
        "status": "INDETERMINATE", "checks": [{"check": "the capture holds a single P4 session", "ok": False, "detail": ""}], "report": None}))
    return p4, tmp_path / "sdio-bbbbbbbbbbbb-c6"


def redo(tmp_path, lines, session):
    p4, c6 = folder(tmp_path, lines, "x")
    return re_.reanalyze(p4, c6, "bench-qualified", p4_session=session, identity=IDENTITY)


def test_each_of_two_complete_plans_can_be_reanalysed_with_its_own_c6_sessions(tmp_path):
    lines = capture(run_lines("clean", "clean"), run_lines("clean", "error"))
    first, second = redo(tmp_path, lines, 1), redo(tmp_path, lines, 2)
    assert first["reanalysed_verdict"]["status"] == "PASS"
    assert first["session_association"]["c6_sessions"] == [1, 2, 3]  # two configurations + confirmation
    assert second["reanalysed_verdict"]["status"] == "PASS"
    assert second["session_association"]["c6_sessions"] == [4, 5, 6]
    assert second["reanalysed_verdict"]["report"]["configs"]["1"]["status"] == "ERROR"  # its own, not the first's


def test_the_complete_plan_is_judged_and_the_started_one_is_listed_not_judged(tmp_path):
    lines = capture(run_lines("clean", "clean"), partial("clean", "clean"))
    first = redo(tmp_path, lines, 1)
    assert first["reanalysed_verdict"]["status"] == "PASS"
    assert first["session_association"]["other_p4_sessions"] == [
        {"session": 2, "complete": False, "configurations": 1, "lines": len(partial("clean", "clean")[0])}]
    assert redo(tmp_path, lines, 2)["reanalysed_verdict"]["status"] == "INDETERMINATE"  # truncated


def test_a_session_must_be_chosen_when_the_capture_holds_several(tmp_path):
    lines = capture(run_lines("clean"), run_lines("clean"))
    p4, c6 = folder(tmp_path, lines, "x")
    with pytest.raises(re_.SessionRequired):
        re_.reanalyze(p4, c6, "bench-qualified", identity=IDENTITY)


def test_a_single_session_needs_no_choice(tmp_path):
    record = redo(tmp_path, capture(run_lines("clean")), None)
    assert record["reanalysed_verdict"]["status"] == "PASS" and record["selected_p4_session"] is None


def test_a_truncated_single_capture_is_indeterminate(tmp_path):
    p4, c6 = split(tb.make(tb.cfgs("clean", "clean"), **PLAN))
    p4 = [x for x in p4 if x.kind != "RESULT"]  # the P4 never reported the end
    assert redo(tmp_path, capture((p4, c6)), None)["reanalysed_verdict"]["status"] == "INDETERMINATE"


def test_the_association_is_ambiguous_when_an_earlier_session_is_truncated(tmp_path):
    first = partial("clean")
    second = run_lines("clean")
    record = redo(tmp_path, capture(first, second), 2)
    assert record["reanalysed_verdict"]["status"] == "INDETERMINATE"
    assert "earlier P4 session is truncated" in record["association_ambiguity"]
    assert record["session_association"] is None


def test_the_association_is_ambiguous_when_the_c6_sessions_are_missing(tmp_path):
    p4, c6 = run_lines("clean")
    last_boot = max(i for i, x in enumerate(c6) if x.kind == "boot")
    record = redo(tmp_path, capture((p4, c6[:last_boot])), 1)  # the confirmation's C6 session is gone
    assert record["reanalysed_verdict"]["status"] == "INDETERMINATE"
    assert "only 1 were captured" in record["association_ambiguity"]


def test_an_unknown_session_number_is_indeterminate(tmp_path):
    record = redo(tmp_path, capture(run_lines("clean"), run_lines("clean")), 3)
    assert record["reanalysed_verdict"]["status"] == "INDETERMINATE"
    assert "there are 2 P4 sessions" in record["association_ambiguity"]


def test_a_foreign_c6_session_ahead_of_the_plan_never_gives_a_pass(tmp_path):
    p4, c6 = run_lines("clean")
    foreign = [tb.L("C6", "boot|firmware=sdio-bench-slave|plan=other")]
    record = redo(tmp_path, capture((p4, foreign + c6)), 1)
    assert record["reanalysed_verdict"]["status"] != "PASS"


def test_headless_lines_are_a_session_of_their_own_and_cannot_be_selected(tmp_path):
    p4, c6 = run_lines("clean")
    headless = [x for x in p4 if x.kind != "boot"]  # the banner was missed
    record = redo(tmp_path, capture((headless, c6)), 1)
    assert record["reanalysed_verdict"]["status"] == "INDETERMINATE"
    assert "no boot banner" in record["association_ambiguity"]


def test_the_reanalysis_file_name_carries_the_session_and_nothing_is_overwritten(tmp_path):
    lines = capture(run_lines("clean"), run_lines("clean"))
    p4, c6 = folder(tmp_path, lines, "x")
    one = re_.write_new(p4, re_.reanalyze(p4, c6, "bench-qualified", p4_session=1, identity=IDENTITY))
    two = re_.write_new(p4, re_.reanalyze(p4, c6, "bench-qualified", p4_session=2, identity=IDENTITY))
    assert one.name.endswith("-p4s1.json") and two.name.endswith("-p4s2.json") and one != two
    with pytest.raises(FileExistsError):
        re_.write_new(p4, re_.reanalyze(p4, c6, "bench-qualified", p4_session=1, identity=IDENTITY))
