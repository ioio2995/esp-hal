"""Analysis of an SDIO throughput benchmark run from the two console logs.

Pure functions. `analyze` turns the `QA|P4|...bench_*` and `QA|C6|...` lines into a report;
`checks` turns the report into verdict checks `(name, ok, kind, detail)` where `kind` is
`"check"` (a demonstrated violation is a FAIL) or `"require"` (missing evidence is INDETERMINATE).

A configuration with any error is never retained as a reliable maximum. A run is a PASS when it
completed and its two logs agree: errors found by the benchmark are results, not failures of the
run. Nothing here is a hardware verdict about the driver.
"""
import re

MODES = ("p4_to_c6", "c6_to_p4", "round_trip")
SENDS = {"p4_to_c6", "round_trip"}  # modes in which the C6 validates packets


def _lines(lines, board, kind):
    return [x for x in lines if x.board == board and x.kind == kind]


def _int(value, default=0, base=10):
    try:
        return int(value, base) if isinstance(value, str) else int(value)
    except (TypeError, ValueError):
        return default


def _measurement(fields):
    return {
        "window": _int(fields.get("window")),
        "phase": _int(fields.get("phase")),
        "packets": _int(fields.get("packets")),
        "useful_bytes": _int(fields.get("useful_bytes")),
        "duration_us": _int(fields.get("duration_us")),
        "goodput_bps": _int(fields.get("goodput_bps")),
        "packets_per_s": _int(fields.get("packets_per_s")),
        "lat_min_us": _int(fields.get("lat_min_us")),
        "lat_avg_us": _int(fields.get("lat_avg_us")),
        "lat_max_us": _int(fields.get("lat_max_us")),
        "not_ready_polls": _int(fields.get("not_ready_polls")),
        "unstable_polls": _int(fields.get("unstable_polls")),
        "c6_packets_ok": _int(fields.get("c6_packets_ok")),
        "c6_bytes_ok": _int(fields.get("c6_bytes_ok")),
    }


def p4_sessions(lines):
    """The P4 lines split at every boot banner: one entry per boot of the P4 firmware.

    Lines that come before the first banner (a capture that missed it) form a session of their
    own, so that they are never silently attached to the next one. Each entry lists its lines and
    whether it reached the end marker and how many configurations it attempted."""
    sessions = []
    for x in lines:
        if x.board != "P4":
            continue
        if x.kind == "boot" or not sessions:
            sessions.append({"lines": [], "headless": x.kind != "boot"})
        sessions[-1]["lines"].append(x)
    for session in sessions:
        session["complete"] = any(x.kind == "RESULT" for x in session["lines"])
        session["configurations"] = sum(1 for x in session["lines"] if x.kind == "bench_config")
    return sessions


def c6_sessions_with_boot(lines):
    """The C6 lines split at every boot banner, the banner included. Lines before the first
    banner are not part of any session."""
    sessions = []
    for x in lines:
        if x.board != "C6":
            continue
        if x.kind == "boot":
            sessions.append([])
        if sessions:
            sessions[-1].append(x)
    return sessions


class AmbiguousAssociation(ValueError):
    """The C6 sessions of a P4 session cannot be determined from the evidence."""


def select_session(lines, number):
    """Returns `(lines_of_the_session, association)` for the 1-based P4 session `number`.

    The P4 resets the C6 once for every configuration it attempts (and once for the
    confirmation), and prints `bench_config` after each reset: the C6 sessions of a P4 session
    are therefore the next `configurations` C6 sessions after those of the earlier P4 sessions.
    That count is only reliable when every earlier P4 session reached its end marker: otherwise
    a reset may or may not have happened before the capture stopped, and the association is
    ambiguous. It must also add up over the whole capture: C6 sessions missing anywhere (or extra)
    make every attribution ambiguous, including that of an apparently complete session."""
    sessions = p4_sessions(lines)
    c6 = c6_sessions_with_boot(lines)
    if not 1 <= number <= len(sessions):
        raise AmbiguousAssociation(f"there are {len(sessions)} P4 sessions, not {number}")
    chosen = sessions[number - 1]
    # The whole capture must add up before anything is attributed: every P4 session asked for its
    # own C6 resets, so the C6 sessions captured must be exactly those (one more is allowed after a
    # truncated last session: its reset may have happened before `bench_config` was printed).
    expected = sum(session["configurations"] for session in sessions)
    allowed = {expected, expected + 1} if not sessions[-1]["complete"] else {expected}
    if len(c6) not in allowed:
        raise AmbiguousAssociation(f"the {len(sessions)} P4 sessions asked for {expected} C6 sessions "
                                   f"but {len(c6)} were captured: the evidence cannot be attributed")
    if chosen["headless"]:
        raise AmbiguousAssociation("the selected P4 session has no boot banner")
    before = sessions[:number - 1]
    if any(not earlier["complete"] or earlier["headless"] for earlier in before):
        raise AmbiguousAssociation("an earlier P4 session is truncated or headless: "
                                   "its C6 resets cannot be counted")
    first = sum(earlier["configurations"] for earlier in before)
    count = chosen["configurations"]
    if count == 0:
        raise AmbiguousAssociation("the selected P4 session attempted no configuration")
    if first + count > len(c6):
        raise AmbiguousAssociation(f"the selected P4 session needs C6 sessions {first + 1}..{first + count} "
                                   f"but only {len(c6)} were captured")
    association = {"p4_session": number, "p4_sessions": len(sessions),
                   "p4_session_complete": chosen["complete"], "c6_sessions_total": len(c6),
                   "c6_sessions": list(range(first + 1, first + count + 1)),
                   "other_p4_sessions": [{"session": i + 1, "complete": s["complete"],
                                          "configurations": s["configurations"], "lines": len(s["lines"])}
                                         for i, s in enumerate(sessions) if i != number - 1]}
    kept = list(chosen["lines"])
    for session in c6[first:first + count]:
        kept += session
    return kept, association


def _c6_sessions(lines):
    """C6 lines split at every boot banner: one session per attempted configuration."""
    sessions, current = [], None
    for x in lines:
        if x.board != "C6":
            continue
        if x.kind == "boot":
            current = []
            sessions.append(current)
        elif current is not None:
            current.append(x)
    return sessions


IDLE_REASON = "receive timed out"


def _failures(session, expected_phases):
    """Splits the failure lines of a C6 session into real failures and the idle timeout.

    The C6 waits for the next control packet; once the P4 has finished with this configuration
    nobody talks to it any more, and after its 30 s timeout it reports `receive timed out`. That
    is the end of the session, not a failure of the measurement, but only when it comes AFTER the
    last phase it served, with the same reason, and every phase the P4 ran was served."""
    served = [_int(x.fields().get("id")) for x in session if x.kind == "bench_phase"]
    last = max((i for i, x in enumerate(session) if x.kind == "bench_phase"), default=-1)
    complete = set(expected_phases) <= set(served)
    real, idle = [], []
    for i, x in enumerate(session):
        if not (x.kind == "fail" or "C6_FAIL" in x.body):
            continue
        (idle if complete and i > last and IDLE_REASON in x.body else real).append(x.body[:80])
    return real, idle


def _records(report):
    """Every measurement of the run: warm-ups, short windows and the confirmation."""
    for e in report["configs"].values():
        for mode in MODES:
            data = e["modes"][mode]
            if data["warmup"]:
                yield e, mode, "warmup", data["warmup"]
            for w in data["windows"]:
                yield e, mode, f"window{w['window']}", w
        for mode, w in e.get("confirm", {}).items():
            yield e, mode, "confirm", w


def analyze(lines):
    """Builds the report. Never raises on a truncated log: what is missing is reported."""
    plan = next(iter(_lines(lines, "P4", "bench_plan")), None)
    plan_fields = plan.fields() if plan else {}
    expected = _int(plan_fields.get("configurations"))
    measurements = _int(plan_fields.get("measurements"))
    confirm_ms = _int(plan_fields.get("confirm_ms"))
    configs = {}
    order = []  # configurations in the order the P4 attempted them (one C6 session each)

    def entry(cfg_id, fields=None):
        if cfg_id not in configs:
            configs[cfg_id] = {
                "id": cfg_id, "status": "INCOMPLETE", "requested_hz": 0, "actual_hz": 0,
                "width": 0, "size": 0, "cmd53": "", "modes": {m: {"warmup": None, "windows": []} for m in MODES},
                "errors": [], "reason": "",
            }
        if fields:
            for key in ("requested_hz", "actual_hz", "width", "size"):
                if fields.get(key) is not None:
                    configs[cfg_id][key] = _int(fields.get(key), configs[cfg_id][key])
        return configs[cfg_id]

    in_confirm = False  # lines after `bench_best` belong to the confirmation session
    for x in lines:
        if x.board != "P4":
            continue
        f = x.fields()
        if in_confirm and x.kind in ("bench_config", "bench_error"):
            f_entry = {k: v for k, v in f.items() if k not in ("requested_hz", "actual_hz", "width", "size")}
        else:
            f_entry = f
        if x.kind == "bench_best":
            in_confirm = True
        elif x.kind == "bench_config":
            e = entry(_int(f.get("cfg")), f_entry)
            if not in_confirm:
                e["cmd53"] = f.get("cmd53", "")
                order.append(e["id"])
        elif x.kind == "bench_warmup":
            if not in_confirm:
                entry(_int(f.get("cfg")), f)["modes"][f.get("mode")]["warmup"] = _measurement(f)
        elif x.kind == "bench_result":
            e = entry(_int(f.get("cfg")), f)
            e["modes"][f.get("mode")]["windows"].append(_measurement(f))
        elif x.kind == "bench_confirm":
            record = _measurement(f)
            # The confirmation's own parameters are kept apart: they never replace the initial ones.
            record["params"] = (_int(f.get("requested_hz")), _int(f.get("actual_hz")),
                                _int(f.get("width")), _int(f.get("size")))
            entry(_int(f.get("cfg"))).setdefault("confirm", {})[f.get("mode")] = record
        elif x.kind == "bench_error":
            e = entry(_int(f.get("cfg")), f_entry)
            reason = re.search(r"reason=([^|]*)", x.body)
            record = {
                "mode": f.get("mode"), "window": _int(f.get("window")),
                "packets_before_error": _int(f.get("packets_before_error")),
                "content": _int(f.get("err_content")), "crc": _int(f.get("err_crc")),
                "r5": _int(f.get("err_r5")), "timeout": _int(f.get("err_timeout")),
                "other": _int(f.get("err_other")), "r5_flags": f.get("r5_flags"),
                "reason": reason.group(1) if reason else "",
            }
            e.setdefault("confirm_errors" if in_confirm else "errors", []).append(record)
        elif x.kind == "bench_skip":
            e = entry(_int(f.get("cfg")), f)
            e["status"], e["reason"] = "SKIPPED", "a lower frequency failed for this width and size"
        elif x.kind == "bench_unsupported":
            if in_confirm:
                continue
            e = entry(_int(f.get("cfg")), f)
            reason = re.search(r"reason=([^|]*)", x.body)
            e["status"], e["reason"] = "UNSUPPORTED", reason.group(1) if reason else ""

    # The confirmation re-runs the best configuration in an extra session: it is the last one.
    confirm_cfg = next((_int(x.fields().get("cfg")) for x in _lines(lines, "P4", "bench_confirmed")), None)
    sessions = _c6_sessions(lines)
    c6_phases = {}
    for session in sessions:
        for x in session:
            if x.kind == "bench_phase":
                c6_phases[_int(x.fields().get("id"))] = x.fields()

    # High speed configuration findings: what the C6 reports (registers at exposure and after each
    # phase) and what the P4 sees (CCCR) before and after the EHS negotiation.
    hs = {
        "c6_exposed": [x.fields() for session in sessions for x in session if x.kind == "bench_regs"],
        "c6_phases": {i: {k: f.get(k) for k in ("shs", "ehs", "hinf_cfg_data1", "slchost_conf")}
                      for i, f in c6_phases.items() if "shs" in f},
        "p4_cccr_speed": [x.fields().get("bus_speed")
                          for x in _lines(lines, "P4", "bench_caps") if "bus_speed" in x.fields()],
        "negotiations": [x.fields() for x in _lines(lines, "P4", "bench_hs")],
    }

    for e in configs.values():
        if e["status"] in ("SKIPPED", "UNSUPPORTED"):
            continue
        if e["errors"]:
            e["status"] = "ERROR"
            continue
        complete = all(
            e["modes"][m]["warmup"] is not None and len(e["modes"][m]["windows"]) == measurements
            for m in MODES
        ) and measurements > 0
        e["status"] = "CLEAN" if complete else "INCOMPLETE"

    for e in configs.values():
        for m in MODES:
            windows = e["modes"][m]["windows"]
            rates = [w["goodput_bps"] for w in windows]
            e["modes"][m]["goodput_mean_bps"] = sum(rates) // len(rates) if rates else 0
            e["modes"][m]["goodput_min_bps"] = min(rates) if rates else 0
            e["modes"][m]["goodput_max_bps"] = max(rates) if rates else 0
            if m == "round_trip" and windows:
                e["modes"][m]["latency_us"] = {
                    "min": min(w["lat_min_us"] for w in windows),
                    "avg": sum(w["lat_avg_us"] for w in windows) // len(windows),
                    "max": max(w["lat_max_us"] for w in windows),
                }

    clean = [e for e in configs.values() if e["status"] == "CLEAN"]
    best = max(clean, key=lambda e: e["modes"]["round_trip"]["goodput_mean_bps"], default=None)
    summary = next(iter(_lines(lines, "P4", "bench_summary")), None)
    found = re.search(r"best_cfg=Some\((\d+)\)", summary.body) if summary else None
    announced = int(found.group(1)) if found else None
    confirmation = None
    if confirm_cfg is not None and confirm_cfg in configs:
        c = configs[confirm_cfg].get("confirm", {})
        confirmation = {
            "cfg": confirm_cfg,
            "modes": c,
            "clean": next(iter(_lines(lines, "P4", "bench_confirmed"))).fields().get("clean") == "true"
            and not configs[confirm_cfg].get("confirm_errors"),
            "errors": configs[confirm_cfg].get("confirm_errors", []),
        }
    return {
        "expected_configurations": expected, "measurements": measurements, "confirm_ms": confirm_ms,
        "plan": plan_fields.get("name"), "highspeed": hs, "configs": configs, "attempt_order": order,
        "c6_sessions": len(sessions), "c6_phases": c6_phases, "c6_session_lines": sessions,
        "best": best["id"] if best else None, "announced_best": announced, "summary_present": summary is not None,
        "confirmation": confirmation,
        "p4_done": bool(_lines(lines, "P4", "RESULT")),
        "capabilities": [x.body for x in _lines(lines, "P4", "bench_caps")],
        "p4_bench_lines": sum(1 for x in lines if x.board == "P4" and x.kind.startswith("bench_")),
    }


HS_LIMIT_HZ = 25_000_000  # above it the host must select high speed (EHS)
SHS_BIT, EHS_BIT, HSPEED_CON_EN_BIT = 1 << 2, 1 << 3, 1 << 27  # HINF_CFG_DATA1 and SLCHOST_CONF


def _hex(value):
    """A register word printed with `{:#010x}`; None when absent or malformed."""
    try:
        return int(value, 16)
    except (TypeError, ValueError):
        return None


def _hs_checks(report):
    """Checks of the `hs` plan: the driver change must take effect and must not degrade the modes
    that do not use high speed. Errors found at a frequency above the limit are results of the
    benchmark and never fail the plan; a demonstrated violation is a FAIL, a missing proof
    INDETERMINATE."""
    hs, configs = report["highspeed"], report["configs"]
    out = []
    # 1. Every configuration that does not use high speed must be clean (400 kHz and 20 MHz).
    slow = {i: e for i, e in configs.items() if 0 < e["actual_hz"] <= HS_LIMIT_HZ}
    demonstrated = [i for i, e in slow.items() if e["status"] in ("ERROR", "UNSUPPORTED")]
    missing = [i for i, e in slow.items() if e["status"] not in ("CLEAN", "ERROR", "UNSUPPORTED")]
    expected_slow = 3  # 400 kHz and 20 MHz in 64 bytes, 20 MHz in 4096 bytes (+ the 400 kHz reference)
    out.append(("every configuration below the high speed limit is clean in the three directions",
                not demonstrated and not missing and len(slow) >= expected_slow,
                "check" if demonstrated else "require",
                {i: e["status"] for i, e in slow.items()}))

    # 2. The registers the C6 reported.
    exposed = hs["c6_exposed"]
    if not exposed:
        out.append(("the C6 reported its registers once the bus was exposed", False, "require", ""))
    for f in exposed:
        cfg, slc = _hex(f.get("hinf_cfg_data1")), _hex(f.get("slchost_conf"))
        out.append(("the exposure registers are complete", cfg is not None and slc is not None, "require", f))
        if cfg is None or slc is None:
            continue
        out.append(("SHS is advertised once the bus is exposed", bool(cfg & SHS_BIT), "check", hex(cfg)))
        out.append(("HSPEED_CON_EN is set once the bus is exposed", bool(slc & HSPEED_CON_EN_BIT), "check", hex(slc)))
        out.append(("EHS is not selected before the host writes it", not cfg & EHS_BIT, "check", hex(cfg)))
        out.append(("the decoded SHS and EHS match the raw word at exposure",
                    (f.get("shs") == "1") == bool(cfg & SHS_BIT) and (f.get("ehs") == "1") == bool(cfg & EHS_BIT),
                    "check", f))
    seen = list(hs["p4_cccr_speed"])
    out.append(("the P4 reads SHS in the CCCR of the C6",
                bool(seen) and all(_int(v.replace("0x", ""), 0, 16) & 1 for v in seen),
                "check" if seen else "require", seen))

    # 3. Every measured phase: the C6 registers agree with the frequency of its configuration.
    wrong, unproven, mismatched = [], [], []
    for e, mode, label, w in _records(report):
        if e["status"] != "CLEAN" or e["actual_hz"] <= 0:
            continue
        regs = hs["c6_phases"].get(w["phase"])
        where = (e["id"], mode, label)
        cfg, slc = (_hex(regs.get("hinf_cfg_data1")), _hex(regs.get("slchost_conf"))) if regs else (None, None)
        if cfg is None or slc is None:
            unproven.append(where)
            continue
        wants_ehs = e["actual_hz"] > HS_LIMIT_HZ
        if bool(cfg & EHS_BIT) != wants_ehs or not cfg & SHS_BIT or not slc & HSPEED_CON_EN_BIT:
            wrong.append((where, {"ehs": bool(cfg & EHS_BIT), "shs": bool(cfg & SHS_BIT),
                                  "hspeed_con_en": bool(slc & HSPEED_CON_EN_BIT), "wants_ehs": wants_ehs}))
        if (regs.get("shs") == "1") != bool(cfg & SHS_BIT) or (regs.get("ehs") == "1") != bool(cfg & EHS_BIT):
            mismatched.append(where)
    out.append(("the C6 registers of every measured phase are proven", not unproven, "require", unproven[:5]))
    out.append(("the C6 selects high speed (EHS) only in the phases above the limit, with SHS and "
                "HSPEED_CON_EN set in all of them", not wrong, "check", wrong[:5]))
    out.append(("the decoded SHS and EHS match the raw words of every phase", not mismatched, "check", mismatched[:5]))

    # 4. The P4 negotiation agrees with what the C6 saw, for the configurations measured above the limit.
    fast = {i: e for i, e in configs.items() if e["actual_hz"] > HS_LIMIT_HZ and e["status"] == "CLEAN"}
    lacking, refused = [], []
    for i, e in fast.items():
        lines = [n for n in hs["negotiations"]
                 if _int(n.get("requested_hz")) == e["requested_hz"] and _int(n.get("size")) == e["size"]]
        if not lines:
            lacking.append(i)
        for n in lines:
            before, after = _hex(n.get("cccr13_before")), _hex(n.get("cccr13_after"))
            if n.get("result") != "accepted" or before is None or after is None or not before & 1 or not after & 2:
                refused.append((i, n))
    out.append(("the P4 negotiated high speed for every configuration measured above the limit",
                not lacking, "require", lacking))
    out.append(("every high speed negotiation was accepted", not refused, "check", refused[:3]))
    quiet = [i for i, e in slow.items() if e["status"] == "CLEAN" and any(
        _int(n.get("requested_hz")) == e["requested_hz"] and _int(n.get("size")) == e["size"]
        for n in hs["negotiations"])]
    out.append(("no high speed negotiation happens at or below the limit", not quiet, "check", quiet))
    return out


def checks(report):
    """Verdict checks `(name, ok, kind, detail)` computed from the report."""
    out = []
    configs = report["configs"]
    n, m = report["expected_configurations"], report["measurements"]
    out.append(("P4 finished the plan", report["p4_done"], "require", ""))
    out.append(("plan header reported", n > 0, "require", n))
    missing = [i for i in range(n) if i not in configs]
    out.append(("every configuration is accounted for", not missing, "require", missing[:10]))
    incomplete = [e["id"] for e in configs.values() if e["status"] == "INCOMPLETE"]
    out.append(("no configuration is left half measured", not incomplete, "require", incomplete[:10]))

    bad_math, bad_bytes, absent, disagreements = [], [], [], []
    for e, mode, label, w in _records(report):
        where = (e["id"], mode, label)
        # 1. goodput is the useful bytes over the effective duration
        expected = w["useful_bytes"] * 1_000_000 // w["duration_us"] if w["duration_us"] else 0
        if abs(expected - w["goodput_bps"]) > 1:
            bad_math.append(where)
        # 2. the useful bytes are the packets x the size x the directions, not a free number
        directions = 2 if mode == "round_trip" else 1
        if w["packets"] < 1 or w["useful_bytes"] != w["packets"] * e["size"] * directions:
            bad_bytes.append(where)
        # 3. the C6 confirms the phase it served (parameters, and what it validated)
        c6 = report["c6_phases"].get(w["phase"])
        if c6 is None:
            absent.append(where)
            continue
        served = (_int(c6.get("count")) == w["packets"] and _int(c6.get("length")) == e["size"]
                  and c6.get("mode") == mode)
        validated = _int(c6.get("packets_ok")) == (w["packets"] if mode in SENDS else 0)
        bytes_ok = _int(c6.get("bytes_ok")) == (w["packets"] * e["size"] if mode in SENDS else 0)
        if (not served or not validated or not bytes_ok or _int(c6.get("content_errors")) != 0
                or w["c6_packets_ok"] != _int(c6.get("packets_ok")) or w["c6_bytes_ok"] != _int(c6.get("bytes_ok"))):
            disagreements.append(where)
    out.append(("goodput equals useful bytes over effective duration", not bad_math, "check", bad_math[:5]))
    out.append(("useful bytes equal packets x size x directions", not bad_bytes, "check", bad_bytes[:5]))
    out.append(("the C6 confirms every phase it served", not absent, "require", absent[:5]))
    out.append(("the C6 and the P4 agree on parameters, packets, bytes and content", not disagreements, "check",
                disagreements[:5]))

    # A configuration the P4 calls clean must have a quiet C6 session.
    sessions = report["c6_session_lines"]
    order = report["attempt_order"]
    contradictions, idle = [], []
    for index, cfg_id in enumerate(order):
        if index >= len(sessions):
            break
        e = configs.get(cfg_id)
        if not e or e["status"] != "CLEAN":
            continue
        phases = [w["phase"] for ee, mode, label, w in _records(report)
                  if ee["id"] == cfg_id and label != "confirm"]
        real, timeouts = _failures(sessions[index], phases)
        if real:
            contradictions.append(cfg_id)
        if timeouts:
            idle.append(cfg_id)
    # The confirmation runs in its own C6 session, right after the ones of the configurations.
    confirmation = report["confirmation"]
    confirm_expected = report["confirm_ms"] > 0 and report["best"] is not None
    confirm_session = sessions[len(order)] if len(sessions) > len(order) else None
    if confirmation and confirm_session is not None:
        phases = [m["phase"] for m in confirmation["modes"].values()]
        real, timeouts = _failures(confirm_session, phases)
        if real:
            contradictions.append("confirmation")
        if timeouts:
            idle.append("confirmation")
    report["c6_idle_timeouts"] = idle
    out.append(("no clean configuration, nor the confirmation, has a failing C6 session", not contradictions,
                "check", contradictions))
    out.append(("one C6 session per attempted configuration", len(sessions) >= len(order), "require",
                (len(sessions), len(order))))
    if confirm_expected:
        out.append(("the confirmation has its own C6 session", confirm_session is not None, "require",
                    (len(sessions), len(order))))

    out.append(("summary reported", report["summary_present"], "require", ""))
    # A disagreement caused by lines that are missing is missing evidence, not a contradiction.
    reliable = not missing and not incomplete and report["summary_present"]
    out.append(("the best configuration is the best clean one", report["announced_best"] == report["best"],
                "check" if reliable else "require", (report["announced_best"], report["best"])))
    if report["confirm_ms"] > 0 and report["best"] is not None:
        c = report["confirmation"]
        out.append(("the best configuration was confirmed", c is not None, "require", ""))
        if c:
            best = configs[report["best"]]
            wanted = (best["requested_hz"], best["actual_hz"], best["width"], best["size"])
            confirmed = {mode: c["modes"].get(mode, {}).get("params") for mode in MODES}
            out.append(("the confirmed configuration is the best one, announced and computed, with the same parameters",
                        c["cfg"] == report["best"] == report["announced_best"]
                        and all(p == wanted for p in confirmed.values()), "check",
                        {"best": report["best"], "announced": report["announced_best"], "confirmed": c["cfg"],
                         "parameters": confirmed}))
            long_enough = all(c["modes"].get(mode, {}).get("duration_us", 0) >= report["confirm_ms"] * 900
                              for mode in MODES)
            out.append(("confirmation ran for the full duration in every direction", long_enough, "check",
                        {mode: c["modes"].get(mode, {}).get("duration_us") for mode in MODES}))
            out.append(("confirmation is error-free", c["clean"], "check", c.get("errors", [])))
    reference = configs.get(0)
    if report["plan"] in ("smoke", "qualified", "full", "hs"):
        # Configuration 0 of these plans is the qualified one (400 kHz, 1 bit, 64 bytes). A run
        # that cannot measure it cleanly in the three directions must not let the campaign go on.
        # A reference configuration that is missing or half measured is missing evidence; only an
        # error found on it is a demonstrated failure.
        demonstrated = reference is not None and reference["status"] in ("ERROR", "SKIPPED", "UNSUPPORTED")
        out.append(("the qualified reference configuration is clean in the three directions",
                    reference is not None and reference["status"] == "CLEAN",
                    "check" if demonstrated else "require",
                    reference and {"status": reference["status"], "errors": reference["errors"][:2]}))
        out.append(("a best configuration exists", report["best"] is not None,
                    "check" if demonstrated else "require", report["best"]))
    if report["plan"] == "hs":
        out += _hs_checks(report)
    lines_bound = 40 + n * (3 * (1 + max(m, 1)) + 4)
    out.append(("no per-packet logging", report["p4_bench_lines"] <= lines_bound, "check",
                (report["p4_bench_lines"], lines_bound)))
    return out


def render_markdown(report):
    """A human-readable summary. Throughput is bytes per second of useful payload."""
    rows = ["| cfg | width | requested Hz | actual Hz | size | status | P4->C6 B/s | C6->P4 B/s | round trip B/s | RT latency avg us | notes |",
            "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    for e in sorted(report["configs"].values(), key=lambda e: e["id"]):
        modes = e["modes"]
        latency = modes["round_trip"].get("latency_us", {})
        note = e["reason"] or "; ".join(
            f"{x['mode']} w{x['window']}: crc={x['crc']} r5={x['r5']} timeout={x['timeout']} content={x['content']} other={x['other']} {x['reason']}"
            for x in e["errors"])
        rows.append("| {id} | {width} | {requested_hz} | {actual_hz} | {size} | {status} | {a} | {b} | {c} | {lat} | {note} |".format(
            a=modes["p4_to_c6"]["goodput_mean_bps"] if e["status"] == "CLEAN" else "",
            b=modes["c6_to_p4"]["goodput_mean_bps"] if e["status"] == "CLEAN" else "",
            c=modes["round_trip"]["goodput_mean_bps"] if e["status"] == "CLEAN" else "",
            lat=latency.get("avg", "") if e["status"] == "CLEAN" else "", note=note, **e))
    best = report["best"]
    text = "\n".join(rows) + f"\n\nBest error-free configuration (round-trip goodput): {best}\n"
    if report["confirmation"]:
        text += f"Confirmation: {report['confirmation']}\n"
    hs = report.get("highspeed")
    if report["plan"] == "hs" and hs:
        text += "\nHigh speed configuration\n"
        text += f"- C6 registers when the bus is exposed: {hs['c6_exposed']}\n"
        text += f"- CCCR 0x13 read by the P4 at the qualified setting: {hs['p4_cccr_speed']}\n"
        text += "".join(f"- EHS negotiation: {n}\n" for n in hs["negotiations"])
        text += "".join(f"- C6 after phase {i}: {r}\n" for i, r in sorted(hs["c6_phases"].items()))
    return text
