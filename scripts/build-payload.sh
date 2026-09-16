#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $# -lt 3 ]]; then
    echo "Usage: scripts/build-payload.sh <payload.raw> <runtime-size> <bootstack-directory> [output-directory] [payload options...]" >&2
    exit 1
fi
task_payload=$1
task_runtime_size=$2
task_bootstack=$3
task_output=${4:-dist}
task_options=()
if [[ $# -gt 4 ]]; then task_options=("${@:5}"); fi
task_usb_uart=false
task_usb_control=false
task_usb_gdb=false
for task_option in "${task_options[@]}"; do
    if [[ $task_option == --usb-uart ]]; then task_usb_uart=true; fi
    if [[ $task_option == --usb-control ]]; then task_usb_control=true; fi
    if [[ $task_option == --usb-gdb ]]; then task_usb_gdb=true; fi
done
mkdir -p "$task_output"
task_run_dir=$(mktemp -d "$task_output/.build.XXXXXX")
trap 'rm -rf "$task_run_dir"' EXIT
cargo build-hv
cargo build -p switchvisor-tool
llvm-objcopy -O binary target/aarch64-unknown-none-softfloat/release/switchvisor "$task_run_dir/bootstrap.raw"
target/debug/switchvisor-tool pack-payload "$task_run_dir/bootstrap.raw" "$task_bootstack" "$task_payload" "$task_runtime_size" "$task_run_dir/bl33.bin" "${task_options[@]}" > "$task_run_dir/manifest.json"
if $task_usb_uart; then
    dtc -@ -I dts -O dtb -o "$task_run_dir/usb-uart.dtbo" config/tegra210-usb-uart.dts
elif $task_usb_control || $task_usb_gdb; then
    dtc -@ -I dts -O dtb -o "$task_run_dir/usb-control.dtbo" config/tegra210-usb-control.dts
fi
python3 - "$task_run_dir/manifest.json" "$task_output" "$task_payload" "$task_bootstack" <<'PY'
import json
import sys
from pathlib import Path

manifest, output, payload, bootstack = map(Path, sys.argv[1:])
names = ["bootstrap.raw", "manifest.json", "bl33.bin", "usb-uart.dtbo", "usb-control.dtbo"]
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
else
    rm -f "$task_output/usb-uart.dtbo"
fi
if [[ -f $task_run_dir/usb-control.dtbo ]]; then
    mv -f "$task_run_dir/usb-control.dtbo" "$task_output/usb-control.dtbo"
    echo "Guest overlay: $task_output/usb-control.dtbo"
else
    rm -f "$task_output/usb-control.dtbo"
fi
mv -f "$task_run_dir/bl33.bin" "$task_output/bl33.bin"
echo "Payload image: $task_output/bl33.bin"
echo "Manifest: $task_output/manifest.json"
