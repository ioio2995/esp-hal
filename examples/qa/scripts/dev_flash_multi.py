#!/usr/bin/env python3
"""Development flash of several images in ONE flash.write through Device Lab, each read back afterwards.

Not a qualification recipe. Safety kept: manifest regions only, ONE flash.write (images written in the given order), no retry,
no restoration, local grant armed by the operator, stop at the first error.

Usage: python dev_flash_multi.py <manifest.json> <device> <addr:file:sha256> [<addr:file:sha256> ...]
"""
import hashlib, json, sys, time
from pathlib import Path
from datetime import datetime, timezone
from iobewi_bench import BenchClient
from iobewi_bench.manifest import CampaignManifest
from iobewi_bench.qualification import Campaign

ROOT = Path('/root/.local/state/iobewi-bench')
EVID = ROOT / 'qualification-evidence' / 'SDIO-A1-P4-001-20261113'


def say(msg):
    print(datetime.now(timezone.utc).strftime('%H:%M:%S'), msg, flush=True)


def main():
    manifest = CampaignManifest(json.loads(Path(sys.argv[1]).read_text()))
    device = sys.argv[2]
    plan = []
    for spec in sys.argv[3:]:
        addr, path, want = spec.split(':')
        data = Path(path).read_bytes()
        assert hashlib.sha256(data).hexdigest() == want, 'image %s does not match its SHA-256' % path
        manifest.check_region(device, int(addr, 0), len(data))
        plan.append((int(addr, 0), data, want))
    config = json.loads((ROOT / 'tokens.json').read_text())
    keys = [k for k, w in config.items() if w.get('id') == 'claude' and w.get('role') == 'client']
    assert len(keys) == 1, 'ambiguous client token'
    client = BenchClient('http://127.0.0.1:8080', keys[0])
    run = EVID / ('dev-multi-' + datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ'))
    campaign = Campaign(client, device, run, Path(__file__), 'DEV-FLASH-MULTI', allow_destructive=True, manifest=manifest,
                        parameters={'images': [(hex(a), len(d), w) for a, d, w in plan]})
    campaign.start()
    try:
        say('WAITING for the operator to arm ONE local grant (>= 200 s) ...')
        campaign.wait_grant(minimum_seconds=200, timeout=300)
        board = campaign.reserve(ttl=1200)
        state = board._check_target()
        assert state['privileged_seconds'] >= 150, 'local grant too short'
        arts = [client.upload(d) for _, d, _ in plan]
        say('writing %s in ONE flash.write' % ', '.join('%d B at %#x' % (len(d), a) for a, d, _ in plan))
        result = board.command('flash.write', {'chip': state['chip'], 'return_console': False,
            'images': [{'address': a, 'artifact': art['id'], 'sha256': art['sha256']} for (a, _, _), art in zip(plan, arts)]},
            timeout=90, allow_destructive=True)
        say('DONE: %s' % json.dumps(result.get('verification')))
        written_op = board.last_operation
        for a, d, _ in plan:
            back = board.read_flash(address=a, size=len(d), timeout=60, return_console=False)
            say('read-back %#x (%d B): %s' % (a, len(d), 'identical' if back == d else 'DIFFERS'))
            if back != d:
                raise RuntimeError('read-back mismatch at %#x' % a)
        board.close()
        say('reservation released; OPERATOR: revoke the local grant ...')
        end = time.monotonic() + 120
        while client.device(device)['privileged_seconds'] != 0 and time.monotonic() < end:
            time.sleep(1)
        campaign.close()
        say('campaign closed')
    except BaseException as error:
        say('STOP: %s: %s ; no retry, board left as is' % (type(error).__name__, str(error)[:300]))
        sys.exit(1)


if __name__ == '__main__':
    main()
