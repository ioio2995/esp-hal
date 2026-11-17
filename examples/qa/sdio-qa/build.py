#!/usr/bin/env python3
"""Build the SDIO QA firmware pairs and write a reproducible artifact manifest.

Run from the repository root:

    python examples/qa/sdio-qa/build.py --output /path/to/private/builds

Each scenario produces an ESP32-P4 host and an ESP32-C6 slave built by `cargo xtask build`
with `SDIO_QA_SCENARIO` set (`SDIO_BENCH_PLAN` for the `bench-*` benchmark scenarios, which use the
separate `sdio-bench-*` firmwares). The app image is produced from the ELF with
`espflash save-image --merge --skip-padding`: a FULL flash-prefix image that starts at flash
address 0 and holds the second-stage bootloader, the partition table and the application. The
bootloader and the table are the ones bundled with espflash, never a custom or erased one, and
nothing is padded to the flash size. This script never opens a serial port and never programs
a board; writing a bootloader and partition table needs the operator's distinct consent.

The manifest records the source SHA, the working-tree state, the proof that the production
driver sources are unchanged from the qualified base, the esp-hal path actually resolved by
Cargo, and the SHA-256 of every ELF and image.
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
import importlib.util

BENCH_PLANS = ("smoke", "qualified", "w1", "w4", "full", "hs")
BENCH_SCENARIOS = tuple("bench-" + plan for plan in BENCH_PLANS)
SCENARIOS = ("h1", "h2", "h3", "h4", "h567", "c6ctl") + BENCH_SCENARIOS  # c6ctl: P4 image only
FIRMWARE = {
    "host": {"bin": "sdio-qa-host", "chip": "esp32p4", "target": "riscv32imafc-unknown-none-elf"},
    "slave": {"bin": "sdio-qa-slave", "chip": "esp32c6", "target": "riscv32imac-unknown-none-elf"},
}
BENCH_FIRMWARE = {
    "host": dict(FIRMWARE["host"], bin="sdio-bench-host"),
    "slave": dict(FIRMWARE["slave"], bin="sdio-bench-slave"),
}


def firmware_for(scenario):
    """Binaries and the environment variable that selects the scenario or the benchmark plan."""
    if scenario.startswith("bench-"):
        return BENCH_FIRMWARE, "SDIO_BENCH_PLAN", scenario[len("bench-"):]
    return FIRMWARE, "SDIO_QA_SCENARIO", scenario


# Paths that make up the production driver. They must not differ from the qualified base.
PRODUCTION_PATHS = [
    "esp-hal", "esp-hal-procmacros", "esp-metadata", "esp-metadata-generated", "esp-println",
    "esp-backtrace", "esp-bootloader-esp-idf", "esp-rtos", "esp-sync", "esp-riscv-rt",
    "esp-rom-sys",
]


def _layout_module():
    spec = importlib.util.spec_from_file_location("sdio_qa_image_layout",
                                                  Path(__file__).with_name("image_layout.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(command, cwd, env=None, check=True):
    result = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True)
    if check and result.returncode:
        sys.stderr.write(result.stdout[-4000:] + result.stderr[-4000:])
        raise SystemExit(f"command failed: {' '.join(command)}")
    return result


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def esp_hal_resolution(root, chip, target):
    """The manifest path and source of the esp-hal package Cargo resolves for this build."""
    out = run(["cargo", "metadata", "--manifest-path", "examples/qa/Cargo.toml", "--format-version",
               "1", "--features", chip, "--filter-platform", target], root).stdout
    package = next(p for p in json.loads(out)["packages"] if p["name"] == "esp-hal")
    return {"manifest_path": package["manifest_path"], "version": package["version"],
            "registry_source": package["source"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--output", required=True, type=Path,
                        help="new private directory for the artifacts (must not exist)")
    parser.add_argument("--base", required=True,
                        help="qualified SDIO commit the production driver must still match")
    parser.add_argument("--p4-flash-size", required=True, help="verified flash size, e.g. 32mb")
    parser.add_argument("--c6-flash-size", required=True, help="verified flash size, e.g. 8mb")
    parser.add_argument("--scenarios", nargs="+", default=list(SCENARIOS), choices=SCENARIOS)
    args = parser.parse_args()
    root = Path(run(["git", "rev-parse", "--show-toplevel"], Path.cwd()).stdout.strip())
    head = run(["git", "rev-parse", "HEAD"], root).stdout.strip()
    base = run(["git", "rev-parse", "--verify", args.base + "^{commit}"], root).stdout.strip()
    dirty = run(["git", "status", "--porcelain", "--untracked-files=all"], root).stdout.splitlines()
    if dirty:
        raise SystemExit("working tree is not clean; commit QA changes first:\n" + "\n".join(dirty))
    production_diff = run(["git", "diff", "--stat", base, "HEAD", "--", *PRODUCTION_PATHS],
                          root).stdout.strip()
    if production_diff:
        raise SystemExit("production sources differ from the qualified base:\n" + production_diff)
    args.output.mkdir(parents=True, mode=0o700)
    manifest = {
        "version": 1,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "source_sha": head,
        "qualified_base_sha": base,
        "production_driver_unchanged_from_base": True,
        "qa_changes": run(["git", "diff", "--name-status", base, "HEAD"], root).stdout.splitlines(),
        "rustc": run(["rustc", "--version", "--verbose"], root).stdout.splitlines(),
        "espflash": run(["espflash", "--version"], root).stdout.strip(),
        "artifacts": [],
    }
    for scenario in args.scenarios:
        firmware, variable, value = firmware_for(scenario)
        for role, spec in firmware.items():
            if scenario == "c6ctl" and role == "slave":
                continue
            env = dict(os.environ, RUST_BACKTRACE="0", **{variable: value})
            command = ["cargo", "xtask", "build", spec["bin"], spec["chip"]]
            run(command, root, env)
            built = root / "target" / spec["target"] / "release" / spec["bin"]
            folder = args.output / scenario
            folder.mkdir(exist_ok=True, mode=0o700)
            elf = folder / f"{spec['bin']}-{scenario}.elf"
            image = folder / f"{spec['bin']}-{scenario}.bin"
            shutil.copy2(built, elf)
            flash_size = args.p4_flash_size if role == "host" else args.c6_flash_size
            run(["espflash", "save-image", "--chip", spec["chip"], "--merge", "--skip-padding",
                 "--flash-size", flash_size, "--skip-update-check", str(elf), str(image)], root)
            layout = _layout_module().parse(image.read_bytes(), spec["chip"])
            manifest["artifacts"].append({
                "scenario": scenario, "role": role, "chip": spec["chip"],
                "target": spec["target"], "scenario_env": f"{variable}={value}",
                "command": " ".join(command), "elf": str(elf.relative_to(args.output)),
                "elf_sha256": sha256(elf), "image": str(image.relative_to(args.output)),
                "image_kind": "full-flash-prefix", "flash_size_arg": flash_size,
                "image_sha256": sha256(image), "image_size": image.stat().st_size,
                "layout": layout,
                "esp_hal": esp_hal_resolution(root, spec["chip"], spec["target"]),
            })
            print(f"built {scenario}/{role}: {manifest['artifacts'][-1]['image_sha256']}")
    manifest_path = args.output / "build-manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print("manifest", manifest_path, sha256(manifest_path))


if __name__ == "__main__":
    main()
