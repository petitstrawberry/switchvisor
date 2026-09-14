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
task_output=${4:-.cache/payload}
task_options=()
if [[ $# -gt 4 ]]; then task_options=("${@:5}"); fi
mkdir -p "$task_output"
task_run_dir=$(mktemp -d "$task_output/build.XXXXXX")
SWITCHVISOR_LINK_BASE=0xB0000000 cargo build-boot
cargo build -p switchvisor-tool
llvm-objcopy -O binary target/aarch64-unknown-none-softfloat/release/switchvisor-boot "$task_run_dir/bootstrap.raw"
target/debug/switchvisor-tool pack-payload "$task_run_dir/bootstrap.raw" "$task_bootstack" "$task_payload" "$task_runtime_size" "$task_run_dir/bl33.bin" "${task_options[@]}" > "$task_run_dir/manifest.json"
echo "Payload image: $task_run_dir/bl33.bin"
echo "Manifest: $task_run_dir/manifest.json"
