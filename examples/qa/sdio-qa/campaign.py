"""Campaign flow of the SDIO hardware recipe, with injectable collaborators.

The pytest recipe passes the real `Campaign`, `Orchestrator` and `Board` objects. The software
tests pass doubles, which is how the safeguards below are demonstrated without hardware:

* the programmed region must be exactly an application partition of the observed table;
* nothing is written unless both boards have two identical backups (and a configured
  historical backup matches);
* a board is restored only if a mutation was engaged on it;
* the campaign stops at the first scenario that is not PASS, before any further programming;
* every operation is preceded by a grant and reservation sized for `Orchestrator.program()`.

No operation is retried.
"""
import base64
import importlib.util
import time
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).parent


def _module(name):
    spec = importlib.util.spec_from_file_location("sdio_qa_" + name, HERE / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verdict = _module("verdict")
inputs = _module("inputs")


class Console:
    """One board's UART events assembled into QA lines for the current scenario."""

    def __init__(self, board):
        self.board, self.cursor = board, None
        self.assembler = verdict.LineAssembler()
        self.raw = bytearray()

    def start(self):
        self.cursor = self.board.cursor()

    def poll(self):
        for event in self.board.events(self.cursor):
            if event["kind"] == "uart":
                data = base64.b64decode(event["data"], validate=True)
                self.raw += data
                self.assembler.feed(data)


@dataclass
class Outcome:
    verdicts: dict = field(default_factory=dict)
    backups: dict = field(default_factory=dict)
    mutated: set = field(default_factory=set)
    restored: dict = field(default_factory=dict)
    cleanup_errors: list = field(default_factory=list)
    error: BaseException | None = None

    @property
    def failed_verdicts(self):
        return {k: v.status for k, v in self.verdicts.items() if v.status != "PASS"}

    @property
    def ok(self):
        return not (self.error or self.cleanup_errors or self.failed_verdicts)


def _prepare(key, params, boards, campaigns, orchestrators, outcome):
    """H0 identification and backups for one board. No write happens here."""
    board, spec = boards[key], params[key]
    identity = board.identify()
    campaigns[key].write("h0-identify.json", {k: identity.get(k) for k in ("chip", "flash_size")})
    if identity["chip"] != spec["chip"] or identity["flash_size"] != spec["flash_size"]:
        raise inputs.InputError(f"{key}: observed identity differs from the parameters")
    table = board.partitions()
    campaigns[key].write("h0-partitions.json", table)
    inputs.check_partition(table, spec)
    reads = []
    for name in ("backup-a.bin", "backup-b.bin"):
        path, meta = orchestrators[key].backup(board, address=spec["app_offset"], size=spec["app_size"],
                                               filename=name, timeout=inputs.RESTORE_TIMEOUT)
        reads.append((path, meta))
    if reads[0][1]["sha256"] != reads[1][1]["sha256"]:
        raise inputs.InputError(f"{key}: two reads of the app partition differ; no write allowed")
    outcome.backups[key] = reads[0][0]
    history = spec.get("historical_backup")
    if history:
        data = Path(history["path"]).read_bytes()
        inputs.check_historical(history, data, Path(reads[0][0]).read_bytes(),
                                spec["app_offset"], spec["app_size"])
        campaigns[key].write("h0-historical-crosscheck.json", {"matches_fresh_backup": True})


def _program(key, scenario, role, params, build, build_dir, boards, campaigns, orchestrators,
             gesture, outcome, ttl):
    row = next(r for r in build["artifacts"] if r["scenario"] == scenario and r["role"] == role)
    gesture(f"{scenario}: put the {key.upper()} in loader mode as declared; "
            "keep the P4 in reset while the C6 is loaded")
    need = inputs.budget(inputs.PROGRAM_TIMEOUT)
    orchestrators[key].wait_ready(timeout=300, grant_seconds=need)
    boards[key].renew(ttl=ttl)
    outcome.mutated.add(key)  # engaged before the call: an uncertain write still needs restoring
    orchestrators[key].program(boards[key], address=params[key]["app_offset"],
                               source=Path(build_dir) / row["image"], sha256=row["image_sha256"],
                               allow_destructive=True, timeout=inputs.PROGRAM_TIMEOUT)
    campaigns[key].write(f"h0-program-{scenario}.json", {
        "image_sha256": row["image_sha256"], "elf_sha256": row["elf_sha256"],
        "address": params[key]["app_offset"], "readback": "independent flash.read equal"})


def _observe(scenario, params, boards, campaigns, make_console, gesture, sleep, clock):
    consoles = {key: make_console(boards[key]) for key in ("p4", "c6")}
    error = None
    try:
        for key, console in consoles.items():
            boards[key].console()
            console.start()
        gesture(f"{scenario}: release the P4 reset / reset the P4 so it boots; do not reset the C6")
        deadline = clock() + params.get("scenario_timeout_seconds", 90)
        while clock() < deadline:
            for console in consoles.values():
                console.poll()
            lines = consoles["p4"].assembler.lines + consoles["c6"].assembler.lines
            if verdict.run_complete(scenario, lines):
                break
            sleep(0.2)
    except BaseException as caught:  # noqa: BLE001 - evidence is saved, then the error continues
        error = caught
        raise
    finally:
        # Partial captures are evidence of the first failure: always keep them, flagged.
        for key, console in consoles.items():
            campaigns[key].write(f"uart-{scenario}-lines.json", {
                "complete_observation": error is None,
                "error": type(error).__name__ if error else None,
                "lines": [[x.board, x.time_ms, x.body] for x in console.assembler.lines]})
            (campaigns[key].directory / f"uart-{scenario}.log").write_bytes(bytes(console.raw))
    return consoles["p4"].assembler.lines + consoles["c6"].assembler.lines


def _cleanup(params, boards, campaigns, orchestrators, gesture, outcome, ttl):
    # Only boards on which a mutation was engaged are restored; a backup alone is not a reason.
    for key in ("c6", "p4"):
        if key not in outcome.mutated:
            continue
        try:
            gesture(f"restore {key.upper()}: put it in loader mode again")
            orchestrators[key].wait_ready(timeout=300, grant_seconds=inputs.budget(inputs.RESTORE_TIMEOUT))
            boards[key].renew(ttl=ttl)
            orchestrators[key].restore(boards[key], outcome.backups[key], allow_destructive=True,
                                       timeout=inputs.RESTORE_TIMEOUT)
            outcome.restored[key] = orchestrators[key].compare(boards[key], outcome.backups[key],
                                                               timeout=inputs.RESTORE_TIMEOUT)
            campaigns[key].write("restoration.json", outcome.restored[key])
        except Exception as error:  # noqa: BLE001 - reported, never hidden, never retried
            outcome.cleanup_errors.append((key, type(error).__name__))
    for key, campaign in campaigns.items():
        try:
            campaign.close()
        except BaseException as error:  # noqa: BLE001
            outcome.cleanup_errors.append((key, type(error).__name__))


def run(params, scenarios, build, build_dir, campaigns, orchestrators, boards, make_console,
        gesture, *, ttl=1800, sleep=time.sleep, clock=time.monotonic):
    outcome = Outcome()
    interrupted = None
    try:
        for key in ("p4", "c6"):
            _prepare(key, params, boards, campaigns, orchestrators, outcome)
        for scenario in scenarios:
            for key, role in (("c6", "slave"), ("p4", "host")):
                _program(key, scenario, role, params, build, build_dir, boards, campaigns,
                         orchestrators, gesture, outcome, ttl)
            lines = _observe(scenario, params, boards, campaigns, make_console, gesture, sleep, clock)
            for test_id, result in verdict.evaluate(scenario, lines).items():
                outcome.verdicts[test_id] = result
                campaigns["p4"].result(test_id, result.status,
                                       preconditions=f"build {params['expected_source_sha']}",
                                       evidence=str(result.checks)[:2000], incidents="",
                                       restoration="pending", limits="app partitions only")
            if outcome.failed_verdicts:
                break  # keep the evidence, program nothing more, then clean up
    except (KeyboardInterrupt, SystemExit) as error:
        interrupted = error
        outcome.error = error
    except Exception as error:  # noqa: BLE001 - recorded; cleanup still runs
        outcome.error = error
    _cleanup(params, boards, campaigns, orchestrators, gesture, outcome, ttl)
    if interrupted:
        raise interrupted
    return outcome
