#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $# -lt 2 ]]; then
    echo "Usage: scripts/build-payload.sh <payload.raw> <runtime-size> [bootstack-directory] [output-directory] [payload options...]" >&2
    exit 1
fi
task_payload=$1
task_runtime_size=$2
task_bootstack=${3:-../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/bootstack}
task_output=${4:-dist}
task_options=()
if [[ $# -gt 4 ]]; then task_options=("${@:5}"); fi
mkdir -p "$task_output"
task_run_dir=$(mktemp -d "$task_output/.build.XXXXXX")
trap 'rm -rf "$task_run_dir"' EXIT
SWITCHVISOR_LINK_BASE=0xFEC00000 cargo build-hv
cargo build -p switchvisor-tool
llvm-objcopy -O binary target/aarch64-unknown-none-softfloat/release/switchvisor "$task_run_dir/bootstrap.raw"
target/debug/switchvisor-tool pack-payload "$task_run_dir/bootstrap.raw" "$task_bootstack" "$task_payload" "$task_runtime_size" "$task_run_dir/bl33.bin" "${task_options[@]}" > "$task_run_dir/manifest.json"
for task_option in "${task_options[@]}"; do
    if [[ $task_option == --usb-uart ]]; then
        dtc -@ -I dts -O dtb -o "$task_run_dir/usb-uart.dtbo" config/tegra210-usb-uart.dts
    fi
done
python3 - "$task_run_dir/manifest.json" "$task_output" "$task_payload" "$task_bootstack" <<'PY'
import json
import sys
from pathlib import Path

manifest, output, payload, bootstack = map(Path, sys.argv[1:])
names = ["bootstrap.raw", "manifest.json", "bl33.bin"]
if (manifest.parent / "usb-uart.dtbo").exists():
    names.append("usb-uart.dtbo")
if any((output / name).is_dir() for name in names):
    sys.exit("An output file path is a directory")
inputs = {p.resolve() for p in [payload, *(bootstack / name for name in ("bl31.bin", "bl33.bin", "nx-plat.dtimg"))]}
if any((output / name).resolve() in inputs for name in names):
    sys.exit("Output directory would overwrite a payload or bootstack input")
report = json.loads(manifest.read_text())
report["output"] = str(output / "bl33.bin")
manifest.write_text(json.dumps(report, indent=2) + "\n")
PY
mv -f "$task_run_dir/bootstrap.raw" "$task_output/bootstrap.raw"
mv -f "$task_run_dir/manifest.json" "$task_output/manifest.json"
if [[ -f $task_run_dir/usb-uart.dtbo ]]; then
    mv -f "$task_run_dir/usb-uart.dtbo" "$task_output/usb-uart.dtbo"
    echo "Guest overlay: $task_output/usb-uart.dtbo"
fi
mv -f "$task_run_dir/bl33.bin" "$task_output/bl33.bin"
echo "Payload image: $task_output/bl33.bin"
echo "Manifest: $task_output/manifest.json"
