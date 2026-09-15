#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $# -lt 3 ]]; then
    echo "Usage: scripts/run-payload.sh <hekate.bin> <payload.raw> <runtime-size> [upload options...]" >&2
    exit 1
fi

task_hekate=$1
task_payload=$2
task_runtime_size=$3
shift 3

if [[ ! -f $task_hekate ]]; then
    echo "Hekate payload not found: $task_hekate" >&2
    exit 1
fi
if [[ ! -f $task_payload ]]; then
    echo "EL1 payload not found: $task_payload" >&2
    exit 1
fi

task_hekate_id=${SWITCHVISOR_HEKATE_ID:-SWV-NX}
if [[ -z $task_hekate_id || ${#task_hekate_id} -gt 7 ]]; then
    echo "SWITCHVISOR_HEKATE_ID must contain 1 to 7 characters" >&2
    exit 1
fi

task_nxboot=${NXBOOT:-nxboot}
if ! command -v "$task_nxboot" >/dev/null 2>&1; then
    echo "nxboot not found; enter the Nix development shell" >&2
    exit 1
fi

task_control=${SWITCHVISORCTL:-target/release/switchvisorctl}
if [[ ! -x $task_control ]]; then
    cargo build -p switchvisorctl --release
fi

"$task_control" reboot-rcm

task_apx_seen=false
for ((task_attempt = 0; task_attempt < 150; task_attempt++)); do
    if ioreg -p IOUSB -l -w 0 2>/dev/null | grep -F '"USB Product Name" = "APX"' >/dev/null; then
        task_apx_seen=true
        break
    fi
    sleep 0.1
done
if [[ $task_apx_seen != true ]]; then
    echo "Nintendo Switch did not enter RCM within 15 seconds" >&2
    exit 1
fi
# IOKit publishes APX before exclusive interface acquisition is consistently ready.
sleep 1

"$task_nxboot" --hekate id "$task_hekate_id" "$task_hekate"
"$task_control" upload-bl33 "$task_payload" --runtime-size "$task_runtime_size" "$@"
"$task_control" boot
