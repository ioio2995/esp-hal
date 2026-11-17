"""Campaign flow of the SDIO hardware recipe, with injectable collaborators.

The pytest recipe passes the real `Campaign`, `Orchestrator` and `Board` objects. The software
tests pass doubles, which is how the safeguards below are demonstrated without hardware:

* the programmed region is computed from the full images (bootloader, table, application at
  flash address 0); a valid table already on the board is never overwritten without the
  operator's explicit consent, and after programming the table read back must be the image's;
* the C6 EN pin is wired to a P4 GPIO: any P4 reset reboots the C6. The P4 therefore runs a
  control firmware (`c6ctl`) first; the C6 is reset by software through it while its GPIO9 is
  grounded by hand (ROM download), all C6 operations are done in one go, and only then is the
  P4 reprogrammed with the scenario image;
* a board is written only after its own two identical backups of that region (and a
  configured historical backup matches);
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


def _prepare(key, params, consent, regions, boards, campaigns, orchestrators, outcome):
    """H0 identification and backups for one board. No write happens here."""
    board, spec = boards[key], params[key]
    address, size = regions[key]
    identity = board.identify()
    campaigns[key].write("h0-identify.json", {k: identity.get(k) for k in ("chip", "flash_size")})
    if identity["chip"] != spec["chip"] or identity["flash_size"] != spec["flash_size"]:
        raise inputs.InputError(f"{key}: observed identity differs from the parameters")
    try:
        table = board.partitions()
    except Exception as error:  # noqa: BLE001 - a blank or erased flash has no valid table
        if "partition_table_invalid" not in str(error):
            raise
        table = None
    campaigns[key].write("h0-existing-partitions.json", {"valid_table": table is not None,
                                                         "table": table})
    inputs.check_existing_table(table, consent)
    reads = []
    for name in ("backup-a.bin", "backup-b.bin"):
        path, meta = orchestrators[key].backup(board, address=address, size=size, filename=name,
                                               timeout=inputs.RESTORE_TIMEOUT)
        reads.append((path, meta))
    if reads[0][1]["sha256"] != reads[1][1]["sha256"]:
        raise inputs.InputError(f"{key}: two reads of the region differ; no write allowed")
    outcome.backups[key] = reads[0][0]
    history = spec.get("historical_backup")
    if history:
        data = Path(history["path"]).read_bytes()
        inputs.check_historical(history, data, Path(reads[0][0]).read_bytes(), address, size)
        campaigns[key].write("h0-historical-crosscheck.json", {"matches_fresh_backup": True})


def _program(key, scenario, role, params, regions, build, build_dir, boards, campaigns,
             orchestrators, gesture, outcome, ttl, ask=True):
    row = next(r for r in build["artifacts"] if r["scenario"] == scenario and r["role"] == role)
    if ask:  # right after its own preparation a board is already in loader mode
        gesture(f"{scenario}: put the {key.upper()} in loader mode as declared; "
                "do not reset the C6 before its programming is finished")
    need = inputs.budget(inputs.PROGRAM_TIMEOUT)
    orchestrators[key].wait_ready(timeout=300, grant_seconds=need)
    boards[key].renew(ttl=ttl)
    outcome.mutated.add(key)  # engaged before the call: an uncertain write still needs restoring
    orchestrators[key].program(boards[key], address=row["layout"]["write_address"],
                               source=Path(build_dir) / row["image"], sha256=row["image_sha256"],
                               allow_destructive=True, timeout=inputs.PROGRAM_TIMEOUT)
    # The independent readback above covers every byte; the table is read as extra evidence.
    inputs.check_written_table(boards[key].partitions(), row["layout"])
    campaigns[key].write(f"h0-program-{scenario}-{role}.json", {
        "image_sha256": row["image_sha256"], "elf_sha256": row["elf_sha256"],
        "address": row["layout"]["write_address"], "size": row["image_size"],
        "layout": row["layout"], "readback": "independent flash.read equal",
        "table_readback": "equals the image's table"})


def _p4_console(state, boards, make_console):
    """The P4 console, opened once: opening the port may reset the P4 and therefore the C6."""
    console = state.get("p4_console")
    if console is None:
        console = make_console(boards["p4"])
        console.start()  # the cursor is armed before the port is opened: opening may reset the P4
        boards["p4"].console()
        state["p4_console"] = console
    return console


def _c6ctl(command, state, boards, orchestrators, make_console, sleep, clock, timeout=30):
    """Sends one line to the P4 control listener and waits for its acknowledgement.

    The listener changes only GPIO54. No retry: a missing acknowledgement is an error.
    """
    console = _p4_console(state, boards, make_console)
    orchestrators["p4"].wait_ready(timeout=300, grant_seconds=inputs.C6CTL_GRANT)
    seen = len(console.assembler.lines)
    boards["p4"].command("uart.write", {"data": base64.b64encode((command + "\n").encode()).decode()})
    deadline = clock() + timeout
    while clock() < deadline:
        console.poll()
        for line in console.assembler.lines[seen:]:
            if line.board == "P4" and line.kind == "c6ctl" and f"ack={command}|" in line.body:
                return line.body
        sleep(0.2)
    raise TimeoutError(f"no acknowledgement of '{command}' from the P4 control listener")


def _c6ctl_ready(state, boards, orchestrators, make_console, sleep, clock, timeout=90):
    """Waits until the control listener answers a harmless `C6 PING`."""
    deadline = clock() + timeout
    while True:
        try:
            return _c6ctl("C6 PING", state, boards, orchestrators, make_console, sleep, clock,
                          timeout=3)
        except TimeoutError:
            if clock() >= deadline:
                raise
            # A PING changes nothing, so asking again is safe; no other command is repeated.


def _c6_into_download(state, boards, orchestrators, make_console, gesture, sleep, clock):
    gesture("ground the C6 GPIO9 (BOOT) by hand now and keep it grounded until told otherwise")
    # The page keeps its loader session open after the first contact. A reset gives the C6 a
    # fresh ROM that this stale session cannot talk to (identify times out with no byte). The
    # `console` command is the only way to leave it: the next loader command then re-synchronizes.
    boards["c6"].console()
    return _c6ctl("C6 RESET", state, boards, orchestrators, make_console, sleep, clock)


RENEW_EVERY = 300  # seconds between two renewals of the reservations during an observation


def _observe(scenario, params, boards, campaigns, make_console, gesture, sleep, clock, state, ttl=1800):
    consoles = {key: make_console(boards[key]) for key in ("p4", "c6")}
    state["p4_console"] = consoles["p4"]  # stays open for the control commands that follow
    error = None
    try:
        # Both cursors are armed before ANY console is opened: opening the P4 port may already
        # boot the P4, which resets the C6, and their boot banners must be captured.
        for console in consoles.values():
            console.start()
        # The C6 console is opened first: opening the P4 port may boot the P4, which resets the C6,
        # and the C6 boot banner is lost if its own port is not open by then.
        for key in ("c6", "p4"):
            boards[key].console()
        gesture(f"{scenario}: reset the P4 so that it boots the scenario (it resets the C6 itself)")
        deadline = clock() + params.get("scenario_timeouts", {}).get(
            scenario, params.get("scenario_timeout_seconds", 90))
        renewed = clock()
        while clock() < deadline:
            if clock() - renewed >= RENEW_EVERY:
                # A long benchmark outlives the reservation TTL. A failed renewal raises: the
                # partial captures are saved as evidence and the cleanup runs, nothing continues.
                for key in ("c6", "p4"):
                    boards[key].renew(ttl=ttl)
                renewed = clock()
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


def _cleanup(params, boards, campaigns, orchestrators, gesture, outcome, ttl, state, make_console,
             sleep, clock):
    # Only boards on which a mutation was engaged are restored; a backup alone is not a reason.
    for key in ("c6", "p4"):
        if key not in outcome.mutated:
            continue
        try:
            if key == "c6":
                # The P4 still runs a firmware with the control listener: reset the C6 by software.
                try:
                    _c6_into_download(state, boards, orchestrators, make_console, gesture, sleep,
                                      clock)
                except Exception:  # noqa: BLE001 - fall back to a manual reset, never skip the restore
                    gesture("could not drive the C6 reset from the P4: put the C6 in ROM download "
                            "by hand (GPIO9 grounded, then reset it)")
            else:
                gesture("restore P4: put it in ROM download again (the C6 may reboot, that is fine)")
            orchestrators[key].wait_ready(timeout=300, grant_seconds=inputs.budget(inputs.RESTORE_TIMEOUT))
            boards[key].renew(ttl=ttl)
            orchestrators[key].restore(boards[key], outcome.backups[key], allow_destructive=True,
                                       timeout=inputs.RESTORE_TIMEOUT)
            outcome.restored[key] = orchestrators[key].compare(boards[key], outcome.backups[key],
                                                               timeout=inputs.RESTORE_TIMEOUT)
            campaigns[key].write("restoration.json", outcome.restored[key])
            if key == "c6":
                gesture("release the C6 GPIO9 (BOOT) now")
                state.pop("p4_console", None)
        except Exception as error:  # noqa: BLE001 - reported, never hidden, never retried
            outcome.cleanup_errors.append((key, type(error).__name__))
    for key, campaign in campaigns.items():
        try:
            campaign.close()
        except BaseException as error:  # noqa: BLE001
            outcome.cleanup_errors.append((key, type(error).__name__))


def run(params, consent, scenarios, regions, build, build_dir, campaigns, orchestrators, boards,
        make_console, gesture, *, ttl=1800, sleep=time.sleep, clock=time.monotonic):
    outcome = Outcome()
    interrupted = None
    state = {}
    args = (params, regions, build, build_dir, boards, campaigns, orchestrators)
    try:
        prepared = set()
        # With `control_firmware_running` the operator declares that the P4 already runs the
        # control firmware: nothing is written to the P4 yet, and its backup is taken later, in
        # ROM download, right before the first scenario image is written.
        if params.get("control_firmware_running") is not True:
            # 1. The P4 is identified and backed up in ROM download; the C6 GPIO9 stays free.
            gesture("put the P4 in ROM download; leave the C6 GPIO9 free (not grounded)")
            _prepare("p4", params, consent, regions, boards, campaigns, orchestrators, outcome)
            prepared.add("p4")
            # 2. The P4 gets the C6 control firmware, then boots it.
            _program("p4", inputs.CONTROL_SCENARIO, "host", *args, gesture, outcome, ttl,
                     ask=False)
            gesture("reset the P4 so that it boots the C6 control firmware")
        _c6ctl_ready(state, boards, orchestrators, make_console, sleep, clock)
        for scenario in scenarios:
            # 3. C6 phase, in one go, driven by the P4: GPIO9 grounded by hand, then EN reset.
            _c6_into_download(state, boards, orchestrators, make_console, gesture, sleep, clock)
            if "c6" not in prepared:
                _prepare("c6", params, consent, regions, boards, campaigns, orchestrators, outcome)
                prepared.add("c6")
            _program("c6", scenario, "slave", *args, gesture, outcome, ttl, ask=False)
            gesture("release the C6 GPIO9 (BOOT) now; the C6 will boot its new firmware when the "
                    "P4 resets it")
            # 4. P4 phase: its own reset reboots the C6, which is already programmed.
            state.pop("p4_console", None)
            gesture(f"{scenario}: put the P4 in ROM download (BOOT + reset)")
            if "p4" not in prepared:
                _prepare("p4", params, consent, regions, boards, campaigns, orchestrators, outcome)
                prepared.add("p4")
            _program("p4", scenario, "host", *args, gesture, outcome, ttl, ask=False)
            lines = _observe(scenario, params, boards, campaigns, make_console, gesture, sleep,
                             clock, state, ttl)
            for test_id, result in verdict.evaluate(scenario, lines).items():
                outcome.verdicts[test_id] = result
                for key in ("p4", "c6"):
                    # The complete verdict, not the truncated evidence of the verdict table.
                    campaigns[key].write(f"verdict-{test_id}.json", result.as_dict())
                    if result.report and result.report.get("markdown"):
                        (campaigns[key].directory / f"report-{test_id}.md").write_text(
                            result.report["markdown"])
                campaigns["p4"].result(test_id, result.status,
                                       preconditions=f"build {params['expected_source_sha']}",
                                       evidence=str(result.checks)[:2000], incidents="",
                                       restoration="pending", limits="flash prefix covered by the images only")
            if outcome.failed_verdicts:
                break  # keep the evidence, program nothing more, then clean up
            if scenario != scenarios[-1]:
                _c6ctl_ready(state, boards, orchestrators, make_console, sleep, clock)
    except (KeyboardInterrupt, SystemExit) as error:
        interrupted = error
        outcome.error = error
    except Exception as error:  # noqa: BLE001 - recorded; cleanup still runs
        outcome.error = error
    _cleanup(params, boards, campaigns, orchestrators, gesture, outcome, ttl, state, make_console,
             sleep, clock)
    if interrupted:
        raise interrupted
    return outcome
