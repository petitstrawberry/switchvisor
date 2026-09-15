#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $# -lt 2 ]]; then
    echo "Usage:" >&2
    echo "  scripts/run-payload.sh <hekate.bin> <payload.raw> <runtime-size> [upload options...]" >&2
    echo "  scripts/run-payload.sh <hekate.bin> --bundle <bundle-directory|bundle.json>" >&2
    exit 1
fi

task_hekate=$1
shift
task_mode=payload
if [[ $1 == --bundle ]]; then
    if [[ $# -ne 2 ]]; then
        echo "--bundle requires exactly one bundle path" >&2
        exit 1
    fi
    task_mode=bundle
    task_bundle=$2
else
    if [[ $# -lt 2 ]]; then
        echo "payload mode requires a raw file and runtime size" >&2
        exit 1
    fi
    task_payload=$1
    task_runtime_size=$2
    shift 2
fi

if [[ ! -f $task_hekate ]]; then
    echo "Hekate payload not found: $task_hekate" >&2
    exit 1
fi
if [[ $task_mode == payload && ! -f $task_payload ]]; then
    echo "EL1 payload not found: $task_payload" >&2
    exit 1
fi
if [[ $task_mode == bundle && ! -e $task_bundle ]]; then
    echo "Guest bundle not found: $task_bundle" >&2
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

if [[ -n ${SWITCHVISORCTL:-} ]]; then
    task_control=$SWITCHVISORCTL
else
    cargo build -p switchvisorctl --release
    task_control=target/release/switchvisorctl
fi
if [[ ! -x $task_control ]]; then
    echo "switchvisorctl is not executable: $task_control" >&2
    exit 1
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
if [[ $task_mode == bundle ]]; then
    "$task_control" deploy "$task_bundle"
else
    "$task_control" upload-bl33 "$task_payload" --runtime-size "$task_runtime_size" "$@"
    "$task_control" boot
fi
