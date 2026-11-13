#!/usr/bin/env python3
"""First hardware trial p4a1-001: ONE flash.write of the sdio-a1-p4-host application at 0x110000 through IOBEWI Device Lab.

Authorised scope (operator message of 2026-11-13): board esp32-local, source 8ac258cc, app.bin SHA-256 297259bf...18fc,
92912 bytes, single flash.write at 0x110000, envelope 0x110000-0x128000. No global erase, no retry, no automatic restoration,
no other region. Any deviation stops the script and leaves the board as it is. Tokens are never printed.

Usage: python p4a1_first_write.py            (the operator has the page connected in ROM download mode and arms ONE local grant)
"""
import hashlib, json, sys, time
from pathlib import Path
from datetime import datetime, timezone
from iobewi_bench import BenchClient
from iobewi_bench.manifest import CampaignManifest
from iobewi_bench.qualification import Campaign
from iobewi_bench.orchestration import Orchestrator

ROOT = Path('/root/.local/state/iobewi-bench')
EVID = ROOT / 'qualification-evidence' / 'SDIO-A1-P4-001-20261113'
ART = ROOT / 'p4-sdio' / 'artifacts' / 'p4a1-001'
BACKUP_A = ROOT / 'qualification-evidence' / 'PC-11-20261113T103326Z' / 'board-0' / 'BACKUP-A.bin'
BACKUP_SHA = 'c2d4ac0f25246b053d4d25f62580578a5f2b2a37002f7cda7a9d3a548702fde0'
APP_SHA = '297259bf7430e225ef8db1f1b81d108507d274d38c907ae0de22cddc3b9218fc'
APP_SIZE = 92912
DEVICE, ADDR = 'esp32-local', 0x110000
ENVELOPE_END = 0x128000
R0, R1 = 0x10d000, 0x12a000           # read window: otadata/phy_init .. 8 KiB beyond the envelope
WRITE_TIMEOUT = 120
URL = 'http://127.0.0.1:8080'


def say(msg):
    print(datetime.now(timezone.utc).strftime('%H:%M:%S'), msg, flush=True)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def token():
    config = json.loads((ROOT / 'tokens.json').read_text())
    keys = [k for k, w in config.items() if w.get('id') == 'claude' and w.get('role') == 'client']
    if len(keys) != 1:
        raise SystemExit('ambiguous client token')
    return keys[0]


def classify(window, backup, app):
    """Compare the read window with the reference backup and the image. Offsets are flash addresses."""
    def seg(lo, hi):
        return window[lo - R0:hi - R0]
    tail_lo = ADDR + APP_SIZE
    out = {}
    out['A_otadata_phy_init_0x10d000-0x110000'] = 'unchanged' if seg(R0, ADDR) == backup[R0:ADDR] else 'DIFFERS'
    out['B_image_0x110000+92912'] = 'equals_app.bin' if seg(ADDR, tail_lo) == app else 'DIFFERS'
    t = seg(tail_lo, 0x127000)
    out['C_tail_to_0x127000'] = 'erased_ff' if t == b'\xff' * len(t) else ('unchanged' if t == backup[tail_lo:0x127000] else 'OTHER')
    d = seg(0x127000, ENVELOPE_END)
    out['D_sector_0x127000'] = ('unchanged_stub_erase' if d == backup[0x127000:ENVELOPE_END] else
                                'erased_ff_rom_fallback_within_envelope' if d == b'\xff' * len(d) else 'OTHER')
    out['E_beyond_envelope_0x128000-0x12a000'] = 'unchanged' if seg(ENVELOPE_END, R1) == backup[ENVELOPE_END:R1] else 'DIFFERS_OUTSIDE_ENVELOPE'
    ok = (out['A_otadata_phy_init_0x10d000-0x110000'] == 'unchanged' and out['B_image_0x110000+92912'] == 'equals_app.bin'
          and out['C_tail_to_0x127000'] in ('erased_ff',) and out['D_sector_0x127000'].startswith(('unchanged', 'erased'))
          and out['E_beyond_envelope_0x128000-0x12a000'] == 'unchanged')
    return out, ok


def main():
    app = (ART / 'app.bin').read_bytes()
    assert len(app) == APP_SIZE and sha(app) == APP_SHA, 'app.bin does not match the authorised image'
    with BACKUP_A.open('rb') as f:
        h = hashlib.sha256()
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    assert h.hexdigest() == BACKUP_SHA, 'BACKUP-A does not match the reference SHA-256'
    with BACKUP_A.open('rb') as f:
        backup = f.read()
    manifest = CampaignManifest(json.loads((EVID / 'manifest.json').read_text()))
    manifest.check_region(DEVICE, ADDR, APP_SIZE)
    say('inputs verified: app.bin and BACKUP-A match their SHA-256; manifest %s' % manifest.sha256[:12])

    client = BenchClient(URL, token())
    health = client.health()
    device = client.device(DEVICE)
    assert device['provenance']['assets_sha256'] == health['build']['assets_sha256'], 'agent assets differ from /health: reload the page'
    manifest.check_device(device)
    assert device['connected'] and not device['quarantined'] and device['reservation'] is None, 'board not ready'

    run = EVID / ('run-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ'))
    campaign = Campaign(client, DEVICE, run, Path(__file__), 'P4A1-WRITE', allow_destructive=True, manifest=manifest,
                        revisions={'firmware_source': '8ac258cc', 'app_sha256': APP_SHA, 'esptool_js': '0.7.0'},
                        parameters={'address': ADDR, 'size': APP_SIZE, 'envelope_end': ENVELOPE_END})
    campaign.start()
    board, summary = None, {'app_sha256': APP_SHA, 'address': hex(ADDR), 'envelope': [hex(ADDR), hex(ENVELOPE_END)]}
    try:
        say('WAITING for the operator to arm ONE local grant (>= 400 s) ...')
        campaign.wait_grant(minimum_seconds=400, timeout=300)
        board = campaign.reserve(ttl=1200)
        say('reserved; reading the reference state %#x-%#x (no write yet)' % (R0, R1))
        pre = board.read_flash(address=R0, size=R1 - R0, timeout=120, return_console=False)
        if pre != backup[R0:R1]:
            summary['stop'] = 'pre-read differs from BACKUP-A: NO write performed'
            raise SystemExit(summary['stop'])
        say('pre-state equals BACKUP-A on the whole window; starting THE single flash.write')
        result = Orchestrator(campaign).program(board, address=ADDR, source=ART / 'app.bin', sha256=APP_SHA,
                                                allow_destructive=True, timeout=WRITE_TIMEOUT)
        summary['write_result'] = {k: result.get(k) for k in ('state', 'flash', 'verification')}
        say('write completed; loader verification: %s' % json.dumps(result.get('verification')))
        post = board.read_flash(address=R0, size=R1 - R0, timeout=120, return_console=False)
        summary['post_window_sha256'] = sha(post)
        summary['classification'], ok = classify(post, backup, app)
        summary['all_checks_ok'] = ok
        say('post-read classification: ' + json.dumps(summary['classification']))
        board.close()
        say('reservation released')
        (run / 'summary.json').write_text(json.dumps(summary, indent=2))
        say('OPERATOR: revoke the local grant now (waiting up to 180 s) ...')
        end = time.monotonic() + 180
        while client.device(DEVICE)['privileged_seconds'] != 0 and time.monotonic() < end:
            time.sleep(1)
        campaign.close()
        say('campaign closed. P4 is still in ROM download mode: you may RESET (without BOOT) with the CH343 terminal open at 115200.')
        sys.exit(0 if ok else 2)
    except SystemExit:
        (run / 'summary.json').write_text(json.dumps(summary, indent=2))
        raise
    except BaseException as error:
        summary['failure'] = {'type': type(error).__name__, 'message': str(error)[:400]}
        (run / 'summary.json').write_text(json.dumps(summary, indent=2))
        say('STOP at first deviation: %s: %s ; no retry, no restoration, board left as is' % (type(error).__name__, str(error)[:300]))
        sys.exit(1)


if __name__ == '__main__':
    main()
