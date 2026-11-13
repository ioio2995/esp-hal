#!/usr/bin/env python3
"""Development flash of an application-only image at 0x110000 through Device Lab (one flash.write + image read-back).

Not a qualification recipe: no reference comparison of neighbouring sectors. Safety kept: manifest envelope
0x110000-0x128000, ONE write, no retry, no restoration, local grant armed by the operator, stop at the first error.

Usage: python p4_dev_flash.py <app.bin> <sha256> [manifest.json]     (board in ROM download mode, native USB, 'Entree manuelle' checked)
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
DEVICE, ADDR = 'esp32-local', 0x110000


def say(msg):
    print(datetime.now(timezone.utc).strftime('%H:%M:%S'), msg, flush=True)


def main():
    path, want = Path(sys.argv[1]), sys.argv[2]
    app = path.read_bytes()
    assert hashlib.sha256(app).hexdigest() == want, 'image does not match the given SHA-256'
    manifest = CampaignManifest(json.loads((EVID / (sys.argv[3] if len(sys.argv) > 3 else 'manifest.json')).read_text()))
    manifest.check_region(DEVICE, ADDR, len(app))
    config = json.loads((ROOT / 'tokens.json').read_text())
    keys = [k for k, w in config.items() if w.get('id') == 'claude' and w.get('role') == 'client']
    assert len(keys) == 1, 'ambiguous client token'
    client = BenchClient('http://127.0.0.1:8080', keys[0])
    run = EVID / ('dev-' + path.parent.name + '-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ'))
    campaign = Campaign(client, DEVICE, run, Path(__file__), 'P4-DEV-FLASH', allow_destructive=True, manifest=manifest,
                        parameters={'address': ADDR, 'size': len(app), 'sha256': want})
    campaign.start()
    try:
        say('WAITING for the operator to arm ONE local grant (>= 200 s) ...')
        campaign.wait_grant(minimum_seconds=200, timeout=300)
        board = campaign.reserve(ttl=1200)
        say('writing %d bytes at %#x (single flash.write)' % (len(app), ADDR))
        result = Orchestrator(campaign).program(board, address=ADDR, source=path, sha256=want, allow_destructive=True, timeout=45)
        say('DONE: %s' % json.dumps(result.get('verification')))
        board.close()
        say('reservation released; OPERATOR: revoke the local grant, then RESET (no BOOT) ...')
        end = time.monotonic() + 180
        while client.device(DEVICE)['privileged_seconds'] != 0 and time.monotonic() < end:
            time.sleep(1)
        campaign.close()
        say('campaign closed')
    except BaseException as error:
        say('STOP: %s: %s ; no retry, board left as is' % (type(error).__name__, str(error)[:300]))
        sys.exit(1)


if __name__ == '__main__':
    main()
