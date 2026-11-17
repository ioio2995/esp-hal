"""Software-only tests of the benchmark evaluator, on synthetic logs shaped like the firmware
output. A PASS here is a simulation: it says nothing about the hardware or its throughput."""
import importlib.util
from pathlib import Path

import pytest

HERE = Path(__file__).parent


def load(name):
    spec = importlib.util.spec_from_file_location("tb_" + name, HERE / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verdict, bench = load("verdict"), load("bench_verdict")
MODES = ("p4_to_c6", "c6_to_p4", "round_trip")
ERR0 = "err_content=0|err_crc=0|err_r5=0|err_timeout=0|err_other=0"


def L(board, body, t=1):
    return verdict.Line(board, t, body)


def result_line(tag, cfg, mode, window, phase, packets, length, micros):
    useful = packets * length * (2 if mode == "round_trip" else 1)
    rate = useful * 1_000_000 // micros
    return L("P4", (f"{tag}|cfg={cfg['id']}|requested_hz={cfg['hz']}|actual_hz={cfg['hz']}|width={cfg['width']}|"
                    f"size={length}|mode={mode}|window={window}|phase={phase}|packets={packets}|useful_bytes={useful}|"
                    f"duration_us={micros}|goodput_bps={rate}|packets_per_s={packets * 1_000_000 // micros}|"
                    f"lat_min_us=100|lat_avg_us=150|lat_max_us=400|{ERR0}|not_ready_polls=0|"
                    f"c6_packets_ok={0 if mode == 'c6_to_p4' else packets}|"
                    f"c6_bytes_ok={0 if mode == 'c6_to_p4' else packets * length}"))


def make(configs, confirm=True, c6_content_errors=0, drop_c6_phase=None, measurements=3, confirm_us=30_500_000,
         confirm_clean=True, extra_p4=0):
    """P4 and C6 lines for the configurations, each `kind` being clean, error, skip or unsupported."""
    p4 = [L("P4", f"bench_plan|name=test|configurations={len(configs)}|module_hz=80000000|measurements={measurements}|"
                  f"window_ms=5000|confirm_ms={30000 if confirm else 0}|warmup_packets=8")]
    c6, phase, best = [], 0, None
    for cfg in configs:
        header = (f"|cfg={cfg['id']}|requested_hz={cfg['hz']}|actual_hz={cfg['hz']}|width={cfg['width']}|size={cfg['size']}")
        if cfg["kind"] == "skip":
            p4.append(L("P4", "bench_skip" + header + "|reason=lower frequency failed"))
            continue
        p4.append(L("P4", f"bench_config|cfg={cfg['id']}|plan=test|requested_hz={cfg['hz']}|actual_hz={cfg['hz']}|"
                          f"width={cfg['width']}|size={cfg['size']}|cmd53=byte|block_size=64"))
        c6.append(L("C6", "boot|firmware=sdio-bench-slave|plan=test"))
        if cfg["kind"] == "unsupported":
            p4.append(L("P4", "bench_unsupported" + header + "|reason=card does not advertise high speed"))
            continue
        for mode in MODES:
            phase += 1
            p4.append(result_line("bench_warmup", cfg, mode, 0, phase, 8, cfg["size"], 10_000))
            if mode != "c6_to_p4":
                c6.append(L("C6", f"bench_phase|id={phase}|mode={mode}|length={cfg['size']}|count=8|packets_ok=8|"
                                  f"bytes_ok={8 * cfg['size']}|content_errors=0|first_error_byte=-1|first_error_seq=-1"))
            if cfg["kind"] == "error" and mode == "c6_to_p4":
                p4.append(L("P4", f"bench_error|cfg={cfg['id']}|requested_hz={cfg['hz']}|actual_hz={cfg['hz']}|"
                                  f"width={cfg['width']}|size={cfg['size']}|mode={mode}|window=1|packets_before_error=17|"
                                  "err_content=0|err_crc=1|err_r5=0|err_timeout=0|err_other=0|r5_flags=0x00|reason="))
                break
            for window in range(1, measurements + 1):
                phase += 1
                micros = 5_000_000 + window * 1000
                p4.append(result_line("bench_result", cfg, mode, window, phase, 1000, cfg["size"], micros))
                if mode != "c6_to_p4" and phase != drop_c6_phase:
                    c6.append(L("C6", f"bench_phase|id={phase}|mode={mode}|length={cfg['size']}|count=1000|"
                                      f"packets_ok=1000|bytes_ok={1000 * cfg['size']}|content_errors={c6_content_errors}|"
                                      "first_error_byte=-1|first_error_seq=-1"))
        if cfg["kind"] == "clean":
            rate = 2000 * cfg["size"] * 1_000_000 // 5_001_000
            if best is None or rate > best[1]:
                best = (cfg, rate)
    p4.append(L("P4", f"bench_summary|measured_clean=1|failed=0|skipped=0|unsupported=0|best_cfg={'Some(%d)' % best[0]['id'] if best else 'None'}"))
    if confirm and best:
        cfg = best[0]
        p4.append(L("P4", f"bench_best|cfg={cfg['id']}|requested_hz={cfg['hz']}|actual_hz={cfg['hz']}|width={cfg['width']}|"
                          f"size={cfg['size']}|round_trip_goodput_bps={best[1]}"))
        p4.append(L("P4", f"bench_config|cfg={cfg['id']}|plan=test|requested_hz={cfg['hz']}|actual_hz={cfg['hz']}|"
                          f"width={cfg['width']}|size={cfg['size']}|cmd53=byte|block_size=64"))
        c6.append(L("C6", "boot|firmware=sdio-bench-slave|plan=test"))
        for mode in MODES:
            phase += 1
            p4.append(result_line("bench_warmup", cfg, mode, 0, phase, 8, cfg["size"], 10_000))
            phase += 1
            p4.append(result_line("bench_confirm", cfg, mode, 1, phase, 6000, cfg["size"], confirm_us))
        p4.append(L("P4", f"bench_confirmed|cfg={cfg['id']}|clean={'true' if confirm_clean else 'false'}"))
    p4 += [L("P4", "bench_noise|x=1")] * 0
    p4 += [L("P4", f"bench_result_flood|n={i}") for i in range(extra_p4)]
    p4.append(L("P4", "RESULT|bench|P4_DONE|plan=test|measured_clean=1|failed=0|skipped=0|confirmed=Some(true)"))
    return p4 + c6


def cfgs(*kinds):
    table = [(400_000, 1, 64), (400_000, 1, 256), (5_000_000, 1, 64), (5_000_000, 1, 256), (10_000_000, 1, 64)]
    return [{"id": i, "hz": table[i][0], "width": table[i][1], "size": table[i][2], "kind": kind}
            for i, kind in enumerate(kinds)]


def evaluate(lines):
    return verdict.evaluate("bench-qualified", lines)["bench-qualified"]


def test_complete_run_with_an_error_a_skip_and_an_unsupported_configuration_passes():
    result = evaluate(make(cfgs("clean", "clean", "error", "skip", "unsupported")))
    assert result.status == "PASS", [c for c in result.checks if not c["ok"]]
    report = result.report
    assert {k: v["status"] for k, v in report["configs"].items()} == {
        "0": "CLEAN", "1": "CLEAN", "2": "ERROR", "3": "SKIPPED", "4": "UNSUPPORTED"}
    assert report["best"] == 1 and report["confirmation"]["clean"] is True


def test_a_configuration_with_an_error_is_never_the_best_even_if_it_is_the_fastest():
    lines = make(cfgs("clean", "clean", "error"))
    assert evaluate(lines).report["best"] != 2  # partial numbers of an aborted configuration never win
    assert evaluate(lines).report["configs"]["2"]["errors"][0]["crc"] == 1


def test_error_report_keeps_the_class_and_the_progress():
    error = evaluate(make(cfgs("clean", "error"))).report["configs"]["1"]["errors"][0]
    assert (error["crc"], error["timeout"], error["r5"], error["content"], error["other"]) == (1, 0, 0, 0, 0)
    assert error["packets_before_error"] == 17 and error["mode"] == "c6_to_p4"


def test_missing_p4_end_marker_is_indeterminate():
    lines = [x for x in make(cfgs("clean", "clean")) if x.kind != "RESULT"]
    assert evaluate(lines).status == "INDETERMINATE"


def test_missing_configuration_is_indeterminate():
    lines = [x for x in make(cfgs("clean", "clean")) if "cfg=1|" not in x.body]
    assert evaluate(lines).status == "INDETERMINATE"


def test_c6_phase_missing_is_indeterminate_never_pass():
    lines = make(cfgs("clean"), drop_c6_phase=2)
    assert evaluate(lines).status == "INDETERMINATE"


def test_c6_content_errors_make_the_run_fail():
    assert evaluate(make(cfgs("clean"), c6_content_errors=2)).status == "FAIL"


def test_goodput_not_matching_bytes_and_duration_fails():
    lines = make(cfgs("clean"))
    lines = [verdict.Line(x.board, 1, x.body.replace("goodput_bps=", "goodput_bps=9")) if x.kind == "bench_result" else x
             for x in lines]
    assert evaluate(lines).status == "FAIL"


def test_announced_best_must_be_the_best_clean_configuration():
    lines = [verdict.Line(x.board, 1, x.body.replace("best_cfg=Some(1)", "best_cfg=Some(0)")) for x in make(cfgs("clean", "clean"))]
    assert evaluate(lines).status == "FAIL"


def test_confirmation_must_last_and_be_clean():
    assert evaluate(make(cfgs("clean"), confirm_us=10_000_000)).status == "FAIL"
    assert evaluate(make(cfgs("clean"), confirm_clean=False)).status == "FAIL"


def test_confirmation_errors_do_not_rewrite_the_initial_measurement():
    lines = make(cfgs("clean", "clean"))
    error = L("P4", "bench_error|cfg=1|requested_hz=400000|actual_hz=400000|width=1|size=256|mode=round_trip|window=1|"
                    "packets_before_error=5|err_content=0|err_crc=0|err_r5=0|err_timeout=1|err_other=0|r5_flags=0x00|reason=")
    index = next(i for i, x in enumerate(lines) if x.kind == "bench_confirmed")
    lines = lines[:index] + [error] + lines[index:]
    lines = [verdict.Line(x.board, 1, x.body.replace("clean=true", "clean=false")) if x.kind == "bench_confirmed" else x for x in lines]
    result = evaluate(lines)
    assert result.report["configs"]["1"]["status"] == "CLEAN"  # the first measurements stay as they were
    assert result.report["confirmation"]["clean"] is False and result.status == "FAIL"


def test_per_packet_logging_is_detected():
    assert evaluate(make(cfgs("clean"), extra_p4=500)).status == "FAIL"
    # the flood lines look like benchmark lines: they are counted by prefix
    assert evaluate(make(cfgs("clean"), extra_p4=0)).status == "PASS"


def test_smoke_plan_has_no_confirmation_and_one_window():
    lines = make(cfgs("clean"), confirm=False, measurements=1)
    assert evaluate(lines).status == "PASS"


def test_run_completes_on_the_p4_marker_only_for_a_benchmark():
    lines = [x for x in make(cfgs("clean")) if x.board == "P4"]
    assert verdict.run_complete("bench-full", lines)
    assert not verdict.run_complete("bench-full", lines[:-1])


def test_markdown_report_lists_every_configuration_and_the_errors():
    text = evaluate(make(cfgs("clean", "error", "skip"))).report["markdown"]
    for token in ("CLEAN", "ERROR", "SKIPPED", "crc=1", "round trip B/s"):
        assert token in text


def test_verdict_dict_keeps_every_check():
    result = evaluate(make(cfgs("clean", "clean")))
    full = result.as_dict()
    assert full["status"] == "PASS" and len(full["checks"]) >= 10 and full["report"]["best"] == 1


def test_c6_failure_in_a_clean_configuration_session_is_a_contradiction():
    lines = make(cfgs("clean", "clean"))
    index = max(i for i, x in enumerate(lines) if x.board == "C6" and x.kind == "boot")
    first_boot = next(i for i, x in enumerate(lines) if x.board == "C6" and x.kind == "boot")
    lines.insert(first_boot + 1, L("C6", "fail|receive timed out|Elapsed"))
    assert evaluate(lines).status == "FAIL"
