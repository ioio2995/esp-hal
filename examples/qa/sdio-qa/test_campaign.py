"""Software-only tests of the campaign flow with doubles. They show the safeguards without any
board; they are simulations and never hardware evidence."""
import base64
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

    def __init__(self, key, calls, spec, world):
        self.key, self.calls, self.spec, self.world = key, calls, spec, world
        self.existing = None
        self.written = TABLE
        self.fail_command = False

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
        self.calls.append(("console", self.key))

    def cursor(self):
        return 0

    def events(self, cursor):
        return []

    def command(self, name, args):
        assert name == "uart.write" and self.key == "p4"
        text = base64.b64decode(args["data"]).decode().strip()
        self.calls.append(("uart.write", "p4", text))
        if not self.fail_command:
            self.world["pending"].append(f"QA|P4|t=1|c6ctl|ack={text}|en=high\r\n")


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
    calls = []
    params = {"expected_source_sha": SHA, "scenario_timeout_seconds": 100,
              "p4": {"device": "p", "chip": "esp32p4", "flash_size": 1 << 25},
              "c6": {"device": "c", "chip": "esp32c6", "flash_size": 1 << 23}}
    build = {"artifacts": [{"scenario": s, "role": r, "image": f"{s}-{r}.bin", "elf_sha256": "0",
                            "image_sha256": "0", "image_size": 4096, "layout": LAYOUT}
                           for s in ("h1", "h2") for r in ("host", "slave")]
             + [{"scenario": "c6ctl", "role": "host", "image": "c6ctl-host.bin", "elf_sha256": "0",
                 "image_sha256": "0", "image_size": 4096, "layout": LAYOUT}]}
    w = {"calls": calls, "params": params, "build": build, "consent": {}, "pending": [],
         "regions": {"p4": (0, 0x20000), "c6": (0, 0x20000)}, "tmp": tmp_path,
         "campaigns": {k: Campaign(tmp_path, k) for k in ("p4", "c6")}}
    w["boards"] = {k: Board(k, calls, params[k], w) for k in ("p4", "c6")}
    w["orchestrators"] = {k: Orchestrator(k, calls, tmp_path) for k in ("p4", "c6")}
    return w


WORLD = {}


class FakeConsole:
    """Scenario lines are delivered at start; control acknowledgements as they are produced."""
    script = {}

    def __init__(self, board):
        self.board = board
        self.assembler = verdict.LineAssembler()
        self.raw = bytearray()

    def start(self):
        for text in self.script.get(self.board.key, []):
            self.assembler.feed(text.encode())

    def poll(self):
        if self.board.key == "p4":
            pending, WORLD["pending"][:] = list(WORLD["pending"]), []
            for text in pending:
                self.assembler.feed(text.encode())


def go(world, scenarios, script=None):
    WORLD.clear()
    WORLD.update(world)
    FakeConsole.script = script or {}
    return flow.run(world["params"], world["consent"], scenarios, world["regions"], world["build"],
                    world["tmp"], world["campaigns"], world["orchestrators"], world["boards"],
                    FakeConsole, lambda label: world["calls"].append(("gesture", label)),
                    sleep=lambda s: None, clock=iter(range(0, 100000, 1)).__next__)


def kinds(world):
    return [c[0] for c in world["calls"]]


def p4_scenario_written(world):
    return [c for c in world["calls"] if c[0] == "program" and c[1] == "p4" and "c6ctl" not in c[2]]


def idx(world, predicate):
    return next(i for i, c in enumerate(world["calls"]) if predicate(c))


def test_order_p4_control_firmware_then_c6_then_p4_scenario(world):
    go(world, ["h1"])
    calls = world["calls"]
    control = idx(world, lambda c: c[:2] == ("program", "p4") and "c6ctl" in c[2])
    reset = idx(world, lambda c: c[:3] == ("uart.write", "p4", "C6 RESET"))
    c6_backup = idx(world, lambda c: c[:2] == ("backup", "c6"))
    c6_program = idx(world, lambda c: c[:2] == ("program", "c6"))
    p4_scenario = idx(world, lambda c: c[:2] == ("program", "p4") and "c6ctl" not in c[2])
    assert control < reset < c6_backup < c6_program < p4_scenario


def test_c6_is_never_reset_by_a_p4_operation_during_its_phase(world):
    go(world, ["h1"])
    calls = world["calls"]
    reset = idx(world, lambda c: c[:3] == ("uart.write", "p4", "C6 RESET"))
    c6_program = idx(world, lambda c: c[:2] == ("program", "c6"))
    between = [c for c in calls[reset:c6_program] if len(c) > 1 and c[1] == "p4" and c[0] != "gesture"]
    assert [c[:3] for c in between] == [("uart.write", "p4", "C6 RESET")]
    assert [c for c in calls[reset:c6_program] if c[0] == "backup" and c[1] == "p4"] == []


def test_stale_c6_loader_session_is_left_before_every_c6_reset(world):
    go(world, ["h1"])
    calls = world["calls"]
    for i, c in enumerate(calls):
        if c[:3] == ("uart.write", "p4", "C6 RESET"):
            assert ("console", "c6") in calls[:i], "console of the C6 must precede the reset"
            assert calls[:i].count(("console", "c6")) >= calls[:i].count(("uart.write", "p4", "C6 RESET")) + 1


def test_gpio9_gestures_frame_the_c6_phase(world):
    go(world, ["h1"])
    labels = [c[1] for c in world["calls"] if c[0] == "gesture"]
    ground = next(i for i, t in enumerate(labels) if "ground the C6 GPIO9" in t)
    release = next(i for i, t in enumerate(labels) if "release the C6 GPIO9" in t)
    assert ground < release


def test_missing_acknowledgement_stops_before_any_c6_write(world):
    world["boards"]["p4"].fail_command = True
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, TimeoutError)
    assert not [c for c in world["calls"] if c[:2] == ("program", "c6") or c[:2] == ("backup", "c6")]
    assert ("restore", "p4") in world["calls"]  # the control firmware was written: restored


def test_every_control_command_is_a_known_listener_command(world):
    go(world, ["h1", "h2"])
    sent = {c[2] for c in world["calls"] if c[0] == "uart.write"}
    assert sent <= {"C6 RESET", "C6 PING", "C6 HOLD", "C6 RELEASE"}


def test_p4_console_is_opened_once_per_p4_boot(world):
    go(world, ["h1"])
    opens = [c for c in world["calls"] if c[:2] == ("console", "p4")]
    # once for the control firmware, once when the scenario image is observed
    assert len(opens) == 2


def test_c6_backup_mismatch_blocks_every_c6_write_and_restores_the_p4(world):
    world["orchestrators"]["c6"].differ = True
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, inputs.InputError)
    assert not [c for c in world["calls"] if c[:2] == ("program", "c6")]
    assert not p4_scenario_written(world)
    assert ("restore", "c6") not in world["calls"] and ("restore", "p4") in world["calls"]
    assert all(c.closed for c in world["campaigns"].values())


def test_p4_backup_mismatch_writes_nothing(world):
    world["orchestrators"]["p4"].differ = True
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, inputs.InputError)
    assert "program" not in kinds(world) and "restore" not in kinds(world)


def test_valid_existing_table_is_not_overwritten_without_consent(world):
    world["boards"]["p4"].existing = [{"address": 0x9000, "type": 1, "size": 0x6000}]
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, inputs.InputError) and "allow_overwrite_valid_table" in str(outcome.error)
    assert "program" not in kinds(world) and "backup" not in kinds(world)


def test_valid_existing_table_with_consent_is_backed_up_then_overwritten(world):
    world["boards"]["p4"].existing = [{"address": 0x9000, "type": 1, "size": 0x6000}]
    world["consent"]["allow_overwrite_valid_table"] = True
    go(world, ["h1"])
    first_backup = idx(world, lambda c: c[:2] == ("backup", "p4"))
    first_write = idx(world, lambda c: c[:2] == ("program", "p4"))
    assert first_backup < first_write


def test_blank_flash_is_accepted_and_both_regions_are_backed_up_twice(world):
    go(world, ["h1"])
    backups = [c[1] for c in world["calls"] if c[0] == "backup"]
    assert backups.count("p4") == 2 and backups.count("c6") == 2


def test_written_table_mismatch_stops_and_restores(world):
    world["boards"]["p4"].written = []
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, inputs.InputError) and "differs from the image" in str(outcome.error)
    assert ("restore", "p4") in world["calls"]


def test_unexpected_partition_read_error_is_not_taken_for_a_blank_flash(world):
    def broken():
        raise RuntimeError("serial_timeout")

    world["boards"]["p4"].partitions = broken
    outcome = go(world, ["h1"])
    assert isinstance(outcome.error, RuntimeError) and "program" not in kinds(world)


def test_first_failing_scenario_stops_programming_and_restores_programmed_boards(world):
    outcome = go(world, ["h1", "h2"])  # empty consoles: H1 is INDETERMINATE, not PASS
    programmed = [c[2] for c in world["calls"] if c[0] == "program"]
    assert not any("h2" in name for name in programmed)
    assert outcome.failed_verdicts and set(outcome.restored) == {"p4", "c6"}
    assert [c[1] for c in world["calls"] if c[0] == "restore"] == ["c6", "p4"]


def test_c6_restore_uses_the_p4_control_listener(world):
    go(world, ["h1"])
    resets = [c for c in world["calls"] if c[:3] == ("uart.write", "p4", "C6 RESET")]
    assert len(resets) == 2  # one before programming, one before the restoration
    restore = idx(world, lambda c: c[:2] == ("restore", "c6"))
    assert idx(world, lambda c: c[:3] == ("uart.write", "p4", "C6 RESET") and c is resets[1]) < restore


def test_only_the_board_with_an_engaged_mutation_is_restored(world):
    def failing(*args, **kwargs):
        raise RuntimeError("write failed")

    world["orchestrators"]["c6"].program = failing
    outcome = go(world, ["h1"])
    assert "c6" in outcome.mutated
    assert [c for c in world["calls"] if c[0] == "restore"] == [("restore", "c6"), ("restore", "p4")]


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
        if self.board.key == "p4":
            pending, WORLD["pending"][:] = list(WORLD["pending"]), []
            for text in pending:
                self.raw += text.encode()
                self.assembler.feed(text.encode())
            for text in self.chunks.get("p4", {}).get(self.polls, []):
                self.raw += text.encode()
                self.assembler.feed(text.encode())
            if self.fail_at is not None and self.polls > self.fail_at and self.board.scenario_phase:
                raise ConnectionError("continuity interrupted")
        else:
            if self.fail_at is not None and self.polls > self.fail_at and self.board.scenario_phase:
                raise ConnectionError("continuity interrupted")
            for text in self.chunks.get(self.board.key, {}).get(self.polls, []):
                self.raw += text.encode()
                self.assembler.feed(text.encode())


def go_scripted(world, scenario, chunks, fail_at=None):
    WORLD.clear()
    WORLD.update(world)
    ScriptedConsole.chunks, ScriptedConsole.fail_at = chunks, fail_at
    for board in world["boards"].values():
        board.scenario_phase = False
    original = world["orchestrators"]["p4"].program

    def program(board, **kw):
        original(board, **kw)
        if "c6ctl" not in str(kw["source"]):
            for b in world["boards"].values():
                b.scenario_phase = True

    world["orchestrators"]["p4"].program = program
    return flow.run(world["params"], world["consent"], [scenario], world["regions"], world["build"],
                    world["tmp"], world["campaigns"], world["orchestrators"], world["boards"],
                    ScriptedConsole, lambda label: None, sleep=lambda s: None,
                    clock=iter(range(0, 100000, 1)).__next__)


def test_partial_uart_is_saved_when_observation_is_interrupted(world):
    line = "QA|C6|t=1|boot|firmware=sdio-qa-slave|scenario=h1\r\n"
    outcome = go_scripted(world, "h1", {"c6": {1: [line]}}, fail_at=1)
    assert isinstance(outcome.error, ConnectionError)
    meta = json.loads((world["campaigns"]["c6"].directory / "uart-h1-lines.json").read_text())
    assert meta["complete_observation"] is False and meta["error"] == "ConnectionError"
    assert (world["campaigns"]["c6"].directory / "uart-h1.log").read_bytes() == line.encode()
    assert set(outcome.restored) == {"p4", "c6"}  # cleanup still ran after the capture


def test_h1_collection_waits_for_the_third_c6_session(world):
    done = "QA|C6|t=1|RESULT|h1|C6_DONE|state=BusExposed\r\n"
    p4 = "QA|P4|t=1|RESULT|h1|P4_PASS\r\n"
    chunks = {"p4": {1: [p4]}, "c6": {1: [done], 3: [done], 5: [done]}}
    outcome = go_scripted(world, "h1", chunks)
    lines = json.loads((world["campaigns"]["c6"].directory / "uart-h1-lines.json").read_text())
    assert lines["complete_observation"] is True
    assert len([x for x in lines["lines"] if x[0] == "C6" and x[2].startswith("RESULT")]) == 3
    assert outcome.error is None


def test_running_control_firmware_skips_its_programming_and_the_first_p4_gestures(world):
    world["params"]["control_firmware_running"] = True
    go(world, ["h1"])
    calls = world["calls"]
    assert not [c for c in calls if c[0] == "program" and "c6ctl" in c[2]]
    first_c6_write = idx(world, lambda c: c[:2] == ("program", "c6"))
    assert not [c for c in calls[:first_c6_write] if c[0] in {"backup", "program"} and c[1] == "p4"]
    labels = [c[1] for c in calls[:first_c6_write] if c[0] == "gesture"]
    assert not any("P4 in ROM download" in t or "boots the C6 control" in t for t in labels)
    # the P4 is backed up, twice, right before its scenario image
    p4_backups = [i for i, c in enumerate(calls) if c[:2] == ("backup", "p4")]
    p4_write = idx(world, lambda c: c[:2] == ("program", "p4"))
    assert len(p4_backups) == 2 and max(p4_backups) < p4_write and min(p4_backups) > first_c6_write
    assert calls[:first_c6_write].count(("uart.write", "p4", "C6 PING")) >= 1


def test_without_the_flag_the_control_firmware_is_written_first(world):
    go(world, ["h1"])
    first_p4_write = idx(world, lambda c: c[:2] == ("program", "p4"))
    assert "c6ctl" in world["calls"][first_p4_write][2]
