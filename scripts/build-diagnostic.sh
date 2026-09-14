#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
task_bootstack=${1:-../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/bootstack}
task_output=${2:-dist/diagnostic}
mkdir -p "$task_output"
task_run_dir=$(mktemp -d "$task_output/.build.XXXXXX")
trap 'rm -rf "$task_run_dir"' EXIT
cargo build-hv
cargo build -p switchvisor-tool
llvm-objcopy -O binary target/aarch64-unknown-none-softfloat/release/switchvisor "$task_run_dir/bootstrap.raw"
target/debug/switchvisor-tool pack-diagnostic "$task_run_dir/bootstrap.raw" "$task_bootstack" "$task_run_dir/bl33.bin" > "$task_run_dir/manifest.json"
python3 - "$task_run_dir/manifest.json" "$task_output" "$task_bootstack" <<'PY'
import json
import sys
from pathlib import Path

manifest, output, bootstack = map(Path, sys.argv[1:])
if any((output / name).is_dir() for name in ("bootstrap.raw", "manifest.json", "bl33.bin")):
    sys.exit("An output file path is a directory")
inputs = {(bootstack / name).resolve() for name in ("bl31.bin", "bl33.bin", "nx-plat.dtimg")}
if any((output / name).resolve() in inputs for name in ("bootstrap.raw", "manifest.json", "bl33.bin")):
    sys.exit("Output directory would overwrite a bootstack input")
report = json.loads(manifest.read_text())
report["output"] = str(output / "bl33.bin")
manifest.write_text(json.dumps(report, indent=2) + "\n")
PY
mv -f "$task_run_dir/bootstrap.raw" "$task_output/bootstrap.raw"
mv -f "$task_run_dir/manifest.json" "$task_output/manifest.json"
mv -f "$task_run_dir/bl33.bin" "$task_output/bl33.bin"
echo "Diagnostic image: $task_output/bl33.bin"
echo "Manifest: $task_output/manifest.json"
