#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
task_bootstack=${1:-../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/bootstack}
task_output=${2:-.cache/diagnostic}
mkdir -p "$task_output"
task_run_dir=$(mktemp -d "$task_output/build.XXXXXX")
cargo build-boot
cargo build -p switchvisor-tool
llvm-objcopy -O binary target/aarch64-unknown-none-softfloat/release/switchvisor-boot "$task_run_dir/bootstrap.raw"
target/debug/switchvisor-tool pack-diagnostic "$task_run_dir/bootstrap.raw" "$task_bootstack" "$task_run_dir/bl33.bin" > "$task_run_dir/manifest.json"
echo "Diagnostic image: $task_run_dir/bl33.bin"
echo "Manifest: $task_run_dir/manifest.json"
