#!/usr/bin/env python3
"""Re-evaluates the captured logs of a finished run with the evaluator of this checkout.

A software verdict can change without flashing a board again. The original verdict is never
rewritten: this tool adds a new file, `reanalysis-<scenario>-<evaluator>.json`, in the P4 evidence
folder. It records which firmware actually ran (the run's source SHA), which evaluator re-read the
logs (commit and file digests), the SHA-256 of every raw input, the original verdict and the new
one. It refuses to overwrite an existing reanalysis and never touches a board or a server.

    python reanalyze.py --p4 EVIDENCE_P4 --c6 EVIDENCE_C6 --scenario bench-smoke
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


def reanalyze(p4_dir, c6_dir, scenario, *, identity=None, now=None):
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
    parameters = p4_dir.parent / "parameters.json"
    result = module.evaluate(scenario, lines)[scenario]
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
        "reanalysed_verdict": {"status": result.status, "checks": result.checks,
                               "failed_checks": [c["check"] for c in result.checks if not c["ok"]],
                               "report": result.report},
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
    target = Path(p4_dir) / f"reanalysis-{record['scenario']}-{tag}.json"
    with open(target, "x") as stream:  # 'x': fails if it exists
        json.dump(record, stream, indent=2, sort_keys=True, default=str)
        stream.write("\n")
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--p4", required=True, type=Path)
    parser.add_argument("--c6", required=True, type=Path)
    parser.add_argument("--scenario", required=True)
    args = parser.parse_args()
    record = reanalyze(args.p4, args.c6, args.scenario)
    print(write_new(args.p4, record))
    print("original:", record["original_verdict"]["status"], "| reanalysed:",
          record["reanalysed_verdict"]["status"])


if __name__ == "__main__":
    sys.exit(main())
