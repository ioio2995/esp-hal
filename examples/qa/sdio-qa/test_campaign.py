"""Software-only tests of the campaign flow with doubles. They show the safeguards without any
board; they are simulations and never hardware evidence."""
import hashlib
import importlib.util
import json
from pathlib import Path

import pytest

spec = importlib.util.spec_from_file_location("sdio_qa_campaign", Path(__file__).with_name("campaign.py"))
flow = importlib.util.module_from_spec(spec)
spec.loader.exec_module(flow)
inputs, verdict = flow.inputs, flow.verdict

SHA = "a" * 40
TABLE = [{"label": "factory", "type": 0, "subtype": 0, "address": 0x10000, "size": 0x100000}]
LAYOUT = {"write_address": 0, "partitions": TABLE}


class Calls(list):
    pass


class Campaign:
    def __init__(self, tmp, name):
        self.directory = tmp / name
        self.directory.mkdir()
        self.closed, self.results = False, []

    def write(self, name, value):
        (self.directory / name).write_text(json.dumps(value, default=str))

    def result(self, test_id, status, **kw):
        self.results.append((test_id, status))

    def close(self):
        self.closed = True


class Board:
    """Blank flash (no valid table) until it is programmed, then the image's table."""

    def __init__(self, key, calls, spec):
        self.key, self.calls, self.spec = key, calls, spec
        self.existing = None  # a valid table present before programming, if a test sets one
        self.written = TABLE

    def identify(self):
        return {"chip": self.spec["chip"], "flash_size": self.spec["flash_size"]}

    def partitions(self):
        if ("program", self.key) in [c[:2] for c in self.calls]:
            return self.written
        if self.existing is None:
            raise RuntimeError("Operation x failed: partition_table_invalid")
        return self.existing

    def renew(self, ttl):
        self.calls.append(("renew", self.key))

    def console(self):
        pass

    def cursor(self):
        return 0

    def events(self, cursor):
        return []


class Orchestrator:
    def __init__(self, key, calls, tmp, differ=False):
        self.key, self.calls, self.tmp, self.differ, self.n = key, calls, tmp, differ, 0
        self.grant = 0

    def backup(self, board, *, address, size, filename, timeout):
        self.n += 1
        data = b"\1" * 16 if not (self.differ and filename == "backup-b.bin") else b"\2" * 16
        path = self.tmp / f"{self.key}-{filename}"
        path.write_bytes(data)
        self.calls.append(("backup", self.key, timeout))
        return path, {"sha256": hashlib.sha256(data).hexdigest()}

    def wait_ready(self, *, timeout, grant_seconds):
        self.grant = grant_seconds

    def program(self, board, *, address, source, sha256, allow_destructive, timeout):
        # Same budget rule as iobewi_bench.Orchestrator.program().
        assert self.grant >= 2 * timeout + 90, "grant too short for programming and readback"
        assert ("renew", self.key) in self.calls
        self.calls.append(("program", self.key, Path(source).name))

    def restore(self, board, backup, *, allow_destructive, timeout):
        assert self.grant >= 2 * timeout + 90, "grant too short for restoration"
        self.calls.append(("restore", self.key))

    def compare(self, board, backup, *, timeout):
        self.calls.append(("compare", self.key))
        return {"equal": True}


@pytest.fixture
def world(tmp_path):
    calls = Calls()
    params = {"expected_source_sha": SHA, "scenario_timeout_seconds": 1,
              "p4": {"device": "p", "chip": "esp32p4", "flash_size": 1 << 25},
              "c6": {"device": "c", "chip": "esp32c6", "flash_size": 1 << 23}}
    build = {"artifacts": [{"scenario": s, "role": r, "image": f"{s}-{r}.bin", "elf_sha256": "0",
                            "image_sha256": "0", "image_size": 4096, "layout": LAYOUT} for s in ("h1", "h2") for r in ("host", "slave")]}
    campaigns = {k: Campaign(tmp_path, k) for k in ("p4", "c6")}
    return {"calls": calls, "params": params, "build": build, "consent": {},
            "regions": {"p4": (0, 0x20000), "c6": (0, 0x20000)}, "tmp": tmp_path, "campaigns": campaigns,
            "boards": {k: Board(k, calls, params[k]) for k in ("p4", "c6")},
            "orchestrators": {k: Orchestrator(k, calls, tmp_path) for k in ("p4", "c6")}}


class FakeConsole:
    script = {}

    def __init__(self, board):
        self.board = board
        self.assembler = verdict.LineAssembler()

    def start(self):
        for text in self.script.get(self.board.key, []):
            self.assembler.feed(text.encode())

    def poll(self):
        pass

    raw = b""


def go(world, scenarios, script=None):
    FakeConsole.script = script or {}
    FakeConsole.raw = bytearray()
    return flow.run(world["params"], world["consent"], scenarios, world["regions"], world["build"],
                    world["tmp"], world["campaigns"],
                    world["orchestrators"], world["boards"], FakeConsole,
                    lambda label: world["calls"].append(("gesture", label)),
                    sleep=lambda s: None, clock=iter(range(0, 1000, 5)).__next__)


def kinds(world):
    return [c[0] for c in world["calls"]]


def test_c6_backup_mismatch_blocks_everything_and_restores_nothing(world):
    world["orchestrators"]["c6"].differ = True
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, inputs.InputError)
    assert "program" not in kinds(world)
    assert "restore" not in kinds(world) and "compare" not in kinds(world)
    assert all(c.closed for c in world["campaigns"].values())


def test_valid_existing_table_is_not_overwritten_without_consent(world):
    world["boards"]["p4"].existing = [{"address": 0x9000, "type": 1, "size": 0x6000}]
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, inputs.InputError) and "allow_overwrite_valid_table" in str(outcome.error)
    assert "program" not in kinds(world)


def test_valid_existing_table_with_consent_is_backed_up_then_overwritten(world):
    world["boards"]["p4"].existing = [{"address": 0x9000, "type": 1, "size": 0x6000}]
    world["consent"]["allow_overwrite_valid_table"] = True
    outcome = go(world, ["h1"])
    assert ("backup", "p4", inputs.RESTORE_TIMEOUT) in world["calls"]
    assert world["calls"].index(("backup", "p4", inputs.RESTORE_TIMEOUT)) < kinds(world).index("program")
    assert outcome.error is None or not isinstance(outcome.error, inputs.InputError)


def test_blank_flash_is_accepted_and_both_regions_are_backed_up_twice(world):
    go(world, ["h1"])
    assert [c[1] for c in world["calls"] if c[0] == "backup"].count("p4") == 2
    assert [c[1] for c in world["calls"] if c[0] == "backup"].count("c6") == 2


def test_written_table_mismatch_stops_and_restores(world):
    world["boards"]["c6"].written = []
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, inputs.InputError) and "differs from the image" in str(outcome.error)
    assert not any(c[0] == "program" and c[1] == "p4" for c in world["calls"])
    assert ("restore", "c6") in world["calls"]


def test_unexpected_partition_read_error_is_not_taken_for_a_blank_flash(world):
    def broken():
        raise RuntimeError("serial_timeout")

    world["boards"]["p4"].partitions = broken
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, RuntimeError) and "program" not in kinds(world)


def test_first_failing_scenario_stops_programming_and_restores_programmed_boards(world):
    outcome = go(world, ["h1", "h2"])  # empty consoles: H1 is INDETERMINATE, not PASS
    programmed = [c for c in world["calls"] if c[0] == "program"]
    assert {c[2] for c in programmed} == {"h1-slave.bin", "h1-host.bin"}
    assert not any("h2" in c[2] for c in programmed)
    assert outcome.failed_verdicts and set(outcome.restored) == {"p4", "c6"}
    assert [c[1] for c in world["calls"] if c[0] == "restore"] == ["c6", "p4"]


def test_only_the_board_with_an_engaged_mutation_is_restored(world):
    orchestrator = world["orchestrators"]["p4"]

    def failing(*args, **kwargs):
        raise RuntimeError("write failed")

    world["orchestrators"]["c6"].program = failing
    outcome = go(world, ["h1"])
    assert outcome.mutated == {"c6"}
    assert [c for c in world["calls"] if c[0] in {"restore", "program"} and c[1] == "p4"] == []
    assert [c for c in world["calls"] if c[0] == "restore"] == [("restore", "c6")]
    assert orchestrator.grant == 0


def test_grant_budget_is_requested_for_programming_and_restoration(world):
    go(world, ["h1"])
    assert world["orchestrators"]["c6"].grant >= inputs.budget(inputs.RESTORE_TIMEOUT)


def test_historical_backup_divergence_blocks_before_programming(world, tmp_path):
    path = tmp_path / "hist.bin"
    path.write_bytes(b"\3" * 0x20000)
    world["params"]["p4"]["historical_backup"] = {"path": str(path),
                                                  "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, inputs.InputError) and "program" not in kinds(world)


class ScriptedConsole:
    """Delivers scripted chunks per poll; may raise to simulate a lost continuity."""
    chunks = {}
    fail_at = None

    def __init__(self, board):
        self.board, self.polls = board, 0
        self.assembler = verdict.LineAssembler()
        self.raw = bytearray()

    def start(self):
        pass

    def poll(self):
        self.polls += 1
        if self.fail_at is not None and self.polls > self.fail_at:
            raise ConnectionError("continuity interrupted")
        for text in self.chunks.get(self.board.key, {}).get(self.polls, []):
            self.raw += text.encode()
            self.assembler.feed(text.encode())


def go_scripted(world, scenario, chunks, fail_at=None):
    ScriptedConsole.chunks, ScriptedConsole.fail_at = chunks, fail_at
    world["params"]["scenario_timeout_seconds"] = 100
    return flow.run(world["params"], world["consent"], [scenario], world["regions"], world["build"],
                    world["tmp"], world["campaigns"],
                    world["orchestrators"], world["boards"], ScriptedConsole, lambda label: None,
                    sleep=lambda s: None, clock=iter(range(0, 10000, 1)).__next__)


def test_partial_uart_is_saved_when_observation_is_interrupted(world):
    line = "QA|C6|t=1|boot|firmware=sdio-qa-slave|scenario=h1\r\n"
    outcome = go_scripted(world, "h1", {"c6": {1: [line]}}, fail_at=1)
    assert isinstance(outcome.error, ConnectionError)
    saved = world["campaigns"]["c6"].directory / "uart-h1.log"
    assert saved.read_bytes() == line.encode()
    meta = json.loads((world["campaigns"]["c6"].directory / "uart-h1-lines.json").read_text())
    assert meta["complete_observation"] is False and meta["error"] == "ConnectionError"
    assert set(outcome.restored) == {"p4", "c6"}  # cleanup still ran after the capture


def test_h1_collection_waits_for_the_third_c6_session(world):
    done = "QA|C6|t=1|RESULT|h1|C6_DONE|state=BusExposed\r\n"
    p4 = "QA|P4|t=1|RESULT|h1|P4_PASS\r\n"
    outcome = go_scripted(world, "h1", {"p4": {1: [p4]}, "c6": {1: [done], 3: [done], 5: [done]}})
    c6_lines = json.loads((world["campaigns"]["c6"].directory / "uart-h1-lines.json").read_text())["lines"]
    assert len([x for x in c6_lines if x[2].startswith("RESULT")]) == 3
    assert outcome.error is None


def test_no_p4_operation_between_c6_preparation_and_c6_programming(world):
    go(world, ["h1"])
    calls = world["calls"]
    first_c6 = next(i for i, c in enumerate(calls) if c[:2] == ("backup", "c6"))
    program_c6 = next(i for i, c in enumerate(calls) if c[:2] == ("program", "c6"))
    assert not [c for c in calls[first_c6:program_c6] if len(c) > 1 and c[1] == "p4"]
    # the operator is asked to put the C6 in download after the P4 backups, before the C6 ones
    last_p4_backup = max(i for i, c in enumerate(calls) if c[:2] == ("backup", "p4"))
    gesture = next(i for i, c in enumerate(calls) if c[0] == "gesture" and "C6 in ROM download" in c[1])
    assert last_p4_backup < gesture < first_c6
