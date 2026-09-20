#!/usr/bin/env python3
"""Exercise EL2 virtio-net queues, management ARP and virtual IRQs without hardware."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("payload", ROOT / "scripts/qemu-payload-smoke.py")
payload = importlib.util.module_from_spec(spec)
spec.loader.exec_module(payload)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bootstrap", type=Path)
    parser.add_argument("bootstack", type=Path)
    parser.add_argument("--output", type=Path, default=Path(".cache/qemu-net"))
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    reports = []
    fixture = (ROOT / "tests/fixtures/net-el1.S").read_text()
    cases = [(mmu, priority) for mmu in [0, 1] for priority in [0x60, 0xa0]]
    for nonce, (mmu, priority) in enumerate(cases, 1):
        name = f"net-mmu{mmu}-priority{priority:02x}"
        raw = output / f"{name}.raw"
        payload.assemble(f".set SV_MMU,{mmu}\n.set SV_NONCE,{nonce}\n"
                         f".set SV_PRIORITY,{priority}\n" + fixture, raw)
        image = output / f"{name}.bin"
        packed = subprocess.run([str(ROOT / "target/debug/switchvisor-tool"), "pack-payload",
                                 str(args.bootstrap.resolve()), str(args.bootstack.resolve()),
                                 str(raw), "0x100000", str(image), "--entry-offset", "64", "--usb-net"],
                                check=True, capture_output=True)
        (output / f"{name}-manifest.json").write_bytes(packed.stdout)
        report = payload.execute_guest(image, raw.read_bytes(), [], nonce, output / name,
                                       result_words=[4, 1, 1, 71, 1, 1, 1, 2, 0x74726976, 15, priority, nonce],
                                       diagnostic_regions=[(0x08000000, 0x1000), (0x08030000, 0x200)])
        report.update({"guest_mmu_enabled": bool(mmu), "physical_usb_emulated": False,
                       "network_virtual_intid": 71, "management_arp_reply_verified": True,
                       "guest_irq_priority": priority, "guest_priority_mask_verified": True})
        reports.append(report)
    (output / "verification.json").write_text(json.dumps(
        {"hardware_validated": False, "cases_passed": len(reports), "cases": reports}, indent=2) + "\n")


if __name__ == "__main__":
    main()
