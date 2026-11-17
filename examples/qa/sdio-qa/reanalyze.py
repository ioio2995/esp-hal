#!/usr/bin/env python3
"""Re-evaluates the captured logs of a finished run with the evaluator of this checkout.

A software verdict can change without flashing a board again. The original verdict is never
rewritten: this tool adds a new file, `reanalysis-<scenario>-<evaluator>.json`, in the P4 evidence
folder. It records which firmware actually ran (the run's source SHA), which evaluator re-read the
logs (commit and file digests), the SHA-256 of every raw input, the original verdict and the new
one. It refuses to overwrite an existing reanalysis and never touches a board or a server.

    python reanalyze.py --p4 EVIDENCE_P4 --c6 EVIDENCE_C6 --scenario bench-smoke [--p4-session N]

A capture can hold several boots of the P4 (a restart, a second plan after the first): the live
evaluation then refuses to judge it (INDETERMINATE). Here ONE session is chosen explicitly with
`--p4-session` (1-based; required when there are several) and judged with the C6 sessions that
belong to it: the P4 resets the C6 once per configuration it attempts, so they are counted from the
earlier sessions. When that count cannot be trusted (an earlier session is truncated, or the C6
sessions are missing) the association is ambiguous and the record says INDETERMINATE without
evaluating anything. The whole capture, the other sessions and the original verdict are kept.
"""
import argparse
import hashlib
import importlib.util
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).parent
EVALUATOR_FILES = ("verdict.py", "bench_verdict.py")


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def _verdict_module():
    spec = importlib.util.spec_from_file_location("sdio_qa_reanalysis_verdict", HERE / "verdict.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def evaluator_identity():
    def git(*args):
        result = subprocess.run(["git", *args], cwd=HERE, text=True, capture_output=True)
        return result.stdout.strip() if result.returncode == 0 else None
    dirty = bool(git("status", "--porcelain", "--", *[str(HERE / f) for f in EVALUATOR_FILES]))
    return {"commit": git("rev-parse", "HEAD"), "evaluator_files_modified": dirty,
            "files": {name: sha256(HERE / name) for name in EVALUATOR_FILES}}


def _bench_module():
    spec = importlib.util.spec_from_file_location("sdio_qa_reanalysis_bench", HERE / "bench_verdict.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SessionRequired(ValueError):
    """The capture holds several P4 sessions and none was chosen."""


def reanalyze(p4_dir, c6_dir, scenario, *, p4_session=None, identity=None, now=None):
    """Returns the reanalysis record (nothing is written)."""
    p4_dir, c6_dir = Path(p4_dir), Path(c6_dir)
    inputs = {}
    lines = []
    module = _verdict_module()
    for key, folder in (("p4", p4_dir), ("c6", c6_dir)):
        saved = folder / f"uart-{scenario}-lines.json"
        raw = folder / f"uart-{scenario}.log"
        data = json.loads(saved.read_text())
        inputs[key] = {"lines_file": str(saved), "lines_sha256": sha256(saved),
                       "raw_log": str(raw), "raw_log_sha256": sha256(raw) if raw.exists() else None,
                       "complete_observation": data.get("complete_observation"), "error": data.get("error")}
        lines += [module.Line(b, t, body) for b, t, body in data["lines"]]
    original = json.loads((p4_dir / f"verdict-{scenario}.json").read_text())
    bench = _bench_module() if scenario.startswith("bench-") else None
    sessions = bench.p4_sessions(lines) if bench else []
    association, ambiguity = None, None
    if bench and (p4_session is not None or len(sessions) > 1):
        if p4_session is None:
            raise SessionRequired(f"the capture holds {len(sessions)} P4 sessions: choose one with --p4-session")
        try:
            lines, association = bench.select_session(lines, p4_session)
        except bench.AmbiguousAssociation as error:
            ambiguity = str(error)
    if ambiguity:
        status, checks, report = "INDETERMINATE", [{"check": "the C6 sessions of the P4 session are determined",
                                                    "ok": False, "detail": ambiguity}], None
        result = None
    else:
        result = module.evaluate(scenario, lines)[scenario]
        status, checks, report = result.status, result.checks, result.report
    identity = identity or evaluator_identity()
    return {
        "kind": "reanalysis",
        "scenario": scenario,
        "created_at": (now or datetime.now(timezone.utc)).isoformat(),
        "note": "Software re-reading of the original captures. The hardware run is unchanged and its "
                "original verdict stands; this record does not turn it into a pass of that revision.",
        "executed_firmware_source_sha": _source_sha(p4_dir),
        "evaluator": identity,
        "inputs": inputs,
        "original_verdict": {"file": str(p4_dir / f"verdict-{scenario}.json"),
                             "sha256": sha256(p4_dir / f"verdict-{scenario}.json"),
                             "status": original["status"],
                             "failed_checks": [c["check"] for c in original["checks"] if not c["ok"]]},
        "p4_sessions_in_capture": len(sessions),
        "selected_p4_session": p4_session,
        "session_association": association,
        "association_ambiguity": ambiguity,
        "limits": ["Session boundaries are the boot banners of the saved capture; the raw log is kept whole.",
                   "The C6 sessions of a P4 session are counted from the earlier sessions (one reset per "
                   "attempted configuration, confirmation included), not read from a link in the logs.",
                   "The association is inferred from the count and the order of the sessions, not proven: the "
                   "phase cross-check does not guarantee to detect a substitution (two C6 sessions with the "
                   "same identifiers, sizes and counters are indistinguishable).",
                   "Only the selected session is judged; the others are listed, not evaluated."],
        "reanalysed_verdict": {"status": status, "checks": checks,
                               "failed_checks": [c["check"] for c in checks if not c["ok"]],
                               "report": report},
    }


def _source_sha(folder):
    """The run's source SHA: the folder name is `sdio-<sha12>-<board>`, and the run manifest has it."""
    for name in ("run.json", "manifest.json"):
        path = Path(folder) / name
        if path.exists():
            text = path.read_text()
            for key in ("source_sha", "expected_source_sha"):
                if key in text:
                    try:
                        found = _find(json.loads(text), key)
                    except ValueError:
                        found = None
                    if found:
                        return found
    return None


def _find(value, key):
    if isinstance(value, dict):
        if key in value and isinstance(value[key], str):
            return value[key]
        for item in value.values():
            found = _find(item, key)
            if found:
                return found
    elif isinstance(value, list):
        for item in value:
            found = _find(item, key)
            if found:
                return found
    return None


def write_new(p4_dir, record):
    """Writes the record to a new file; an existing reanalysis is never replaced."""
    tag = (record["evaluator"]["commit"] or "unknown")[:12]
    session = f"-p4s{record['selected_p4_session']}" if record.get("selected_p4_session") else ""
    target = Path(p4_dir) / f"reanalysis-{record['scenario']}-{tag}{session}.json"
    with open(target, "x") as stream:  # 'x': fails if it exists
        json.dump(record, stream, indent=2, sort_keys=True, default=str)
        stream.write("\n")
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--p4", required=True, type=Path)
    parser.add_argument("--c6", required=True, type=Path)
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--p4-session", type=int, default=None,
                        help="1-based P4 session to judge (required when the capture holds several)")
    args = parser.parse_args()
    try:
        record = reanalyze(args.p4, args.c6, args.scenario, p4_session=args.p4_session)
    except SessionRequired as error:
        raise SystemExit(str(error))
    print(write_new(args.p4, record))
    print("original:", record["original_verdict"]["status"], "| reanalysed:",
          record["reanalysed_verdict"]["status"])


if __name__ == "__main__":
    sys.exit(main())
