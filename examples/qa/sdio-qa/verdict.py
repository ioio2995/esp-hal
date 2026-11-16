"""Verdicts for the SDIO QA scenarios, computed from the two console logs.

The firmwares print `QA|<P4|C6>|t=<ms>|<kind>|key=value|...` lines. This module reassembles
them from UART chunks, then checks both firmwares' observations against each scenario
contract. It never infers success from a missing line: absent evidence is `INDETERMINATE`.

Statuses: `PASS`, `FAIL` (a contract violation or a firmware-reported failure) and
`INDETERMINATE` (evidence missing or truncated). `BLOCKED` is a campaign decision for a
scenario that cannot be built or started and is never produced here.
"""
import re
from dataclasses import dataclass, field

LINE = re.compile(r"QA\|(P4|C6)\|t=(\d+)\|(.*)")
OK_REQUEST = "length=64|present=true|first_mismatch=None"


@dataclass(frozen=True)
class Line:
    board: str
    time_ms: int
    body: str

    def fields(self):
        """`key=value` fields of the body; the first segment is the event kind."""
        result = {}
        for part in self.body.split("|")[1:]:
            for token in part.split():
                if "=" in token:
                    key, _, value = token.partition("=")
                    result[key] = value
        return result

    @property
    def kind(self):
        return self.body.split("|", 1)[0]


class LineAssembler:
    """Joins UART chunks into lines. A line split across chunks is completed by the next one."""

    def __init__(self):
        self._buffer = ""
        self.lines = []

    def feed(self, data: bytes):
        self._buffer += data.decode("utf-8", errors="replace").replace("\r", "")
        *complete, self._buffer = self._buffer.split("\n")
        for text in complete:
            match = LINE.search(text)
            if match:
                self.lines.append(Line(match.group(1), int(match.group(2)), match.group(3)))


@dataclass
class Verdict:
    status: str = "PASS"
    checks: list = field(default_factory=list)

    def check(self, name, ok, detail=""):
        self.checks.append({"check": name, "ok": bool(ok), "detail": str(detail)})
        if not ok and self.status == "PASS":
            self.status = "FAIL"

    def require(self, name, present, detail=""):
        """Evidence that must exist; its absence is INDETERMINATE, never PASS."""
        self.checks.append({"check": name, "ok": bool(present), "detail": str(detail)})
        if not present and self.status == "PASS":
            self.status = "INDETERMINATE"


def _of(lines, board, kind, contains=""):
    return [x for x in lines if x.board == board and x.kind == kind and contains in x.body]


def _payload_mismatches(lines):
    return [x.body for x in lines if "first_mismatch=Some" in x.body or "present=false" in x.body]


def _common(verdict, lines, scenario, c6_boots):
    verdict.check("no payload mismatch", not _payload_mismatches(lines), _payload_mismatches(lines)[:3])
    boots = _of(lines, "C6", "boot", f"scenario={scenario}")
    verdict.require("C6 boot banner", len(boots) >= 1, len(boots))
    verdict.check("C6 reboot count", not boots or len(boots) == c6_boots, len(boots))
    verdict.require("P4 boot banner", len(_of(lines, "P4", "boot", f"scenario={scenario}")) == 1)
    failures = [x.body for x in lines if x.kind in {"fail", "cmd_error", "stop_failed"}
                or "_FAIL" in x.body]
    verdict.check("no firmware-reported failure", not failures, failures[:3])
    verdict.require("P4 result", bool(_of(lines, "P4", "RESULT", "P4_PASS")))
    verdict.require("C6 result", bool(_of(lines, "C6", "RESULT", "C6_DONE")))


def _values(lines, board, kind, key, contains=""):
    return [x.fields().get(key) for x in _of(lines, board, kind, contains)]


def _h1(lines):
    v = Verdict()
    _common(v, lines, "h1", 3)
    sessions = _of(lines, "P4", "SESSION")
    v.require("three fresh sessions passed", [x.body for x in sessions] ==
              [f"SESSION|h1|{n}|PASS" for n in (1, 2, 3)], len(sessions))
    v.require("C6 validated 64 bytes three times",
              len(_of(lines, "C6", "rx_check", OK_REQUEST)) == 3)
    v.require("C6 transmitted 64 bytes three times",
              _values(lines, "C6", "tx_done", "dma_bytes") == ["64"] * 3)
    v.require("P4 validated 64 bytes three times",
              len(_of(lines, "P4", "response_check", "length=64|first_mismatch=None")) == 3)
    v.check("credit 1 and length 64 in every session",
            _values(lines, "P4", "credit_observed", "cum") == ["1"] * 3 and
            _values(lines, "P4", "length_observed", "cum") == ["64"] * 3)
    return v


def _h2(lines):
    v = Verdict()
    _common(v, lines, "h2", 1)
    v.require("C6 validated three requests",
              len(_of(lines, "C6", "rx_check", OK_REQUEST)) == 3)
    v.check("credits advance one per packet",
            _values(lines, "P4", "credit_observed", "cum") == ["1", "2", "3"],
            _values(lines, "P4", "credit_observed", "cum"))
    v.check("published length advances 64 per packet",
            _values(lines, "P4", "length_observed", "cum") == ["64", "128", "192"],
            _values(lines, "P4", "length_observed", "cum"))
    v.check("host consumed exactly 64 bytes per response",
            _values(lines, "P4", "response_consumed", "bytes_consumed_cum") == ["64", "128", "192"])
    v.check("host consumed three credits", _values(lines, "P4", "progress", "credits_consumed") ==
            ["1", "2", "3"])
    v.require("P4 validated three responses",
              len(_of(lines, "P4", "response_check", "length=64|first_mismatch=None")) == 3)
    v.check("slave announced credits 1, 2, 3",
            [x.fields().get("token") for x in lines if x.board == "C6" and x.kind == "diag"
             and x.body.split("|")[1] == "rx_armed"] == ["1", "2", "3"])
    v.check("slave published 64, 128, 192",
            [x.fields().get("published") for x in lines if x.board == "C6" and x.kind == "diag"
             and x.body.split("|")[1] == "tx_armed"] == ["64", "128", "192"])
    v.check("direction changes without leaving BusExposed",
            _values(lines, "C6", "round_end", "state") == ["BusExposed"] * 3)
    return v


def _h3(lines):
    v = Verdict()
    _common(v, lines, "h3", 1)
    v.check("slave published exactly 65 bytes", _values(lines, "P4", "length_observed", "cum")
            == ["65"], _values(lines, "P4", "length_observed", "cum"))
    v.require("host received 65 matching bytes",
              len(_of(lines, "P4", "response_check", "length=65|first_mismatch=None")) == 1)
    v.check("host consumed 65 bytes", _values(lines, "P4", "response_consumed",
            "bytes_consumed_cum") == ["65"])
    v.check("slave DMA transmitted 65 bytes", _values(lines, "C6", "tx_done", "dma_bytes") == ["65"])
    v.check("no receive credit published", _values(lines, "P4", "credit_observed", "cum") == [])
    return v


def _h4(lines):
    v = Verdict()
    _common(v, lines, "h4", 1)
    for direction in ("rx", "tx"):
        v.require(f"{direction} wait started then cancelled",
                  len(_of(lines, "C6", "cancel", f"|{direction}|")) == 1)
        v.check(f"{direction} DMA still armed, no extra credit/length",
                [x.body.rsplit("=", 1)[1] for x in _of(lines, "C6", "cancel_check",
                 f"|{direction}|")] == ["true"])
    v.require("C6 validated 64 bytes after the resumed wait",
              len(_of(lines, "C6", "rx_check", OK_REQUEST)) == 1)
    v.check("slave completed the resumed transmit", _values(lines, "C6", "tx_done", "dma_bytes")
            == ["64"])
    waits = [int(w) for w in _values(lines, "P4", "ready_observed", "waited_ms")]
    v.check("host saw readiness only after both cancel windows",
            len(waits) == 2 and all(w >= 250 for w in waits), waits)
    v.check("host credit and length stayed exact",
            _values(lines, "P4", "credit_observed", "cum") == ["1"] and
            _values(lines, "P4", "length_observed", "cum") == ["64"])
    v.require("P4 validated the response",
              len(_of(lines, "P4", "response_check", "length=64|first_mismatch=None")) == 1)
    return v


def _stop(lines, tag, direction):
    v = Verdict()
    v.require(f"{tag}: premature restitution refused",
              any(x.body.endswith("true") for x in _of(lines, "C6", "pre_stop", f"|{tag}|")))
    stop = _of(lines, "C6", "stop", f"|{tag}|")
    v.require(f"{tag}: stop result reported", len(stop) == 1)
    v.check(f"{tag}: stop confirmed and session invalidated",
            len(stop) == 1 and "result=Ok(())" in stop[0].body
            and "state=SessionInvalidated" in stop[0].body, [x.body for x in stop])
    after = [x for x in lines if x.board == "C6" and x.kind == "diag"
             and x.body.split("|")[1] == "after_stop" and direction in x.body]
    v.require(f"{tag}: DMA PARK observed after stop",
              any("park=true" in x.body for x in after), "register observation")
    v.check(f"{tag}: storage returned after the stop only",
            len(_of(lines, "C6", "restitution", f"|{tag}|")) == 1)
    reopen = _of(lines, "C6", "reopen", f"|{tag}|")
    v.check(f"{tag}: enable_bus refuses the instance",
            len(reopen) == 1 and "Err(SessionInvalidated)" in reopen[0].body)
    v.require(f"{tag}: host observed the stop",
              len(_of(lines, "P4", "stop_observed", f"|{tag}|")) == 1)
    return v


def _h567(lines):
    result = {}
    h5 = _stop(lines, "h5", "slc0_tx")
    h5.check("h5: host credit 1 visible, nothing consumed",
             any(x.body.startswith("h5|armed|credit_cum=1|length_cum=0")
                 for x in _of(lines, "P4", "h5")))
    h6 = _stop(lines, "h6", "slc0_rx")
    h6.check("h6: published length 64 visible, nothing read",
             any(x.body.startswith("h6|published|credit_cum=0|length_cum=64")
                 for x in _of(lines, "P4", "h6")))
    h7 = Verdict()
    h7.require("h7: new session fresh counters", any(
        x.fields().get("length_cum") == "0" and x.fields().get("credit_cum") in {"0", "1"}
        for x in _of(lines, "P4", "h7", "fresh_session")))
    h7.require("h7: round trip validated by the slave",
               len(_of(lines, "C6", "rx_check", "k=7|" + OK_REQUEST)) == 1)
    h7.require("h7: round trip validated by the host",
               len(_of(lines, "P4", "response_check", "length=64|first_mismatch=None")) == 1)
    observed = _values(lines, "P4", "credit_observed", "cum")
    h7.check("h7: first credit of the new session is 1", observed[-1:] == ["1"], observed)
    h7.check("h7: published length of the new session is 64",
             _values(lines, "P4", "length_observed", "cum")[-1:] == ["64"])
    for name, verdict in (("h5", h5), ("h6", h6), ("h7", h7)):
        _common_tail(verdict, lines)
        result[name] = verdict
    return result


def _common_tail(verdict, lines):
    verdict.check("no payload mismatch", not _payload_mismatches(lines), _payload_mismatches(lines)[:3])
    failures = [x.body for x in lines if x.kind in {"fail", "cmd_error", "stop_failed"}
                or "_FAIL" in x.body]
    verdict.check("no firmware-reported failure", not failures, failures[:3])
    verdict.require("P4 result", bool(_of(lines, "P4", "RESULT", "P4_PASS")))
    verdict.require("C6 result", bool(_of(lines, "C6", "RESULT", "C6_DONE")))


def evaluate(scenario, lines):
    """Returns `{test_id: Verdict}` for the scenario (`h567` yields h5, h6 and h7)."""
    if scenario == "h567":
        return _h567(lines)
    return {scenario: {"h1": _h1, "h2": _h2, "h3": _h3, "h4": _h4}[scenario](lines)}
