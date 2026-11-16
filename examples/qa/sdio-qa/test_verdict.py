"""Software-only tests of the verdict logic. They use synthetic logs shaped like the firmware
output; a PASS here is a simulation and never evidence about hardware."""
import importlib.util
from pathlib import Path

import pytest

spec = importlib.util.spec_from_file_location("sdio_qa_verdict", Path(__file__).with_name("verdict.py"))
verdict = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verdict)

OK_RX = "rx_check|k={k}|length=64|present=true|first_mismatch=None"


def log(*rows):
    assembler = verdict.LineAssembler()
    for board, body in rows:
        assembler.feed(f"QA|{board}|t=1|{body}\r\n".encode())
    return assembler.lines


def boots(scenario, c6=1):
    return [("P4", f"boot|firmware=sdio-qa-host|scenario={scenario}")] + \
        [("C6", f"boot|firmware=sdio-qa-slave|scenario={scenario}")] * c6


def end(scenario):
    return [("C6", f"RESULT|{scenario}|C6_DONE|state=BusExposed"),
            ("P4", f"RESULT|{scenario}|P4_PASS")]


def p4_round(k, credit=1, length=64, consumed=64, waits=None):
    rows = [("P4", f"credit_observed|cum={credit}|ior1=false"),
            ("P4", f"ready_observed|waited_ms={waits[0] if waits else 10}"),
            ("P4", f"length_observed|cum={length}|ior1=false"),
            ("P4", f"ready_observed|waited_ms={waits[1] if waits else 10}"),
            ("P4", "response_check|response|length=64|first_mismatch=None|tail=[ff]"),
            ("P4", f"response_consumed|response|bytes=64|bytes_consumed_cum={consumed}|length_cum={length}")]
    return rows


def h1():
    rows = []
    for n in (1, 2, 3):
        rows += boots("h1")[1:] if n > 1 else boots("h1")
        rows += [("C6", OK_RX.format(k=0)), ("C6", "tx_done|k=0|dma_bytes=64")]
        rows += p4_round(0)
        rows += [("P4", f"SESSION|h1|{n}|PASS")]
    return log(*rows, *end("h1"))


def h2():
    rows = boots("h2")
    for k in (1, 2, 3):
        rows += [("C6", f"diag|rx_armed|slc0_tx(host->slave) dw0=0x80000040 token={k} park=false"),
                 ("C6", OK_RX.format(k=k)),
                 ("C6", f"diag|tx_armed|slc0_rx(slave->host) dw0=0x80000040 published={64 * k} park=false"),
                 ("C6", f"round_end|k={k}|state=BusExposed")]
        rows += p4_round(k, credit=k, length=64 * k, consumed=64 * k)
        rows += [("P4", f"progress|k={k}|credits_consumed={k}|bytes_consumed={64 * k}")]
    return log(*rows, *end("h2"))


def h3():
    rows = boots("h3") + [("C6", "tx_done|dma_bytes=65"),
        ("P4", "length_observed|cum=65|ior1=false"), ("P4", "ready_observed|waited_ms=1"),
        ("P4", "response_check|odd_response|length=65|first_mismatch=None|tail=[ee]"),
        ("P4", "response_consumed|odd_response|bytes=65|bytes_consumed_cum=65|length_cum=65")]
    return log(*rows, *end("h3"))


def h4():
    rows = boots("h4") + [("C6", "cancel|rx|wait pending then cancelled"),
        ("C6", "cancel_check|rx|dma_still_armed_and_no_extra_credit=true"),
        ("C6", OK_RX.format(k=0)),
        ("C6", "cancel|tx|wait pending then cancelled"),
        ("C6", "cancel_check|tx|dma_still_armed_and_no_extra_length=true"),
        ("C6", "tx_done|k=0|dma_bytes=64")] + p4_round(0, waits=(310, 305))
    return log(*rows, *end("h4"))


def stop(tag, direction):
    return [("C6", f"pre_stop|{tag}|restitution_refused_while_dma_active=true"),
            ("C6", f"diag|after_stop|{direction} dw0=0x0 park=true"),
            ("C6", f"stop|{tag}|result=Ok(())|state=SessionInvalidated|duration_us=5"),
            ("C6", f"restitution|{tag}|other_direction_empty=true"),
            ("C6", f"reopen|{tag}|result=Err(SessionInvalidated)"),
            ("P4", f"stop_observed|{tag}|via=no_bus_response")]


def h567():
    rows = boots("h567") + stop("h5", "slc0_tx(host->slave)") + stop("h6", "slc0_rx(slave->host)")
    rows += [("P4", "h5|armed|credit_cum=1|length_cum=0"), ("P4", "credit_observed|cum=1|ior1=false"),
             ("P4", "h6|published|credit_cum=0|length_cum=64"),
             ("P4", "length_observed|cum=64|ior1=false"),
             ("P4", "h7|fresh_session|credit_cum=0|length_cum=0"), ("C6", OK_RX.format(k=7))]
    rows += p4_round(7)
    return log(*rows, *end("h567"))


@pytest.mark.parametrize("scenario,build", [("h1", h1), ("h2", h2), ("h3", h3), ("h4", h4)])
def test_golden_logs_pass(scenario, build):
    result = verdict.evaluate(scenario, build())
    assert {k: v.status for k, v in result.items()} == {scenario: "PASS"}, result


def test_h567_reports_three_verdicts():
    result = verdict.evaluate("h567", h567())
    assert {k: v.status for k, v in result.items()} == {"h5": "PASS", "h6": "PASS", "h7": "PASS"}


def test_h5_h6_h7_are_independent():
    rows = [x for x in h567() if "|h6|" not in x.body]
    result = verdict.evaluate("h567", rows)
    assert result["h5"].status == "PASS"
    assert result["h6"].status == "INDETERMINATE"


def test_mismatch_is_a_failure_not_indeterminate():
    rows = h1()
    index = next(i for i, x in enumerate(rows) if x.kind == "response_check")
    rows[index] = verdict.Line("P4", 1, "response_check|response|length=64|first_mismatch=Some(3)|tail=[]")
    assert verdict.evaluate("h1", rows)["h1"].status == "FAIL"


def test_stale_credit_fails_h2():
    rows = [x for x in h2() if x.body != "credit_observed|cum=3|ior1=false"]
    rows.append(verdict.Line("P4", 1, "credit_observed|cum=4|ior1=false"))
    assert verdict.evaluate("h2", rows)["h2"].status == "FAIL"


def test_missing_firmware_result_is_indeterminate_never_pass():
    rows = [x for x in h3() if x.kind != "RESULT"]
    assert verdict.evaluate("h3", rows)["h3"].status == "INDETERMINATE"


def test_firmware_failure_line_fails():
    rows = h4() + log(("C6", "fail|arm_receive|InvalidState"))
    assert verdict.evaluate("h4", rows)["h4"].status == "FAIL"


def test_early_readiness_fails_h4():
    rows = [x for x in h4() if not x.body.startswith("ready_observed")]
    rows += log(("P4", "ready_observed|waited_ms=2"), ("P4", "ready_observed|waited_ms=2"))
    assert verdict.evaluate("h4", rows)["h4"].status == "FAIL"


def test_h3_requires_exactly_65_bytes():
    rows = [verdict.Line(x.board, 1, x.body.replace("65", "68")) for x in h3()]
    assert verdict.evaluate("h3", rows)["h3"].status != "PASS"


def test_unrefused_reopen_fails():
    rows = [verdict.Line(x.board, 1, x.body.replace("Err(SessionInvalidated)", "Ok(())")
                         if x.kind == "reopen" else x.body) for x in h567()]
    assert verdict.evaluate("h567", rows)["h5"].status == "FAIL"


def test_assembler_joins_split_chunks_and_ignores_noise():
    assembler = verdict.LineAssembler()
    for chunk in (b"boot noise\nQA|C6|t=5|bo", b"ot|firmware=x|scenario=h1\r\nQA|P4|t=6|x"):
        assembler.feed(chunk)
    assert [(x.board, x.body) for x in assembler.lines] == [("C6", "boot|firmware=x|scenario=h1")]
