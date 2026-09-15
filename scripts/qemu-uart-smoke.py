#!/usr/bin/env python3
"""Exercise virtual UART, USB MMIO ownership and WFI/vGIC IRQs on QEMU A57."""
import argparse
import importlib.util
import json
from pathlib import Path
import struct
import subprocess

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("stage2_smoke", ROOT / "scripts/qemu-stage2-smoke.py")
stage2 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stage2)
payload = stage2.payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bootstrap", type=Path)
    parser.add_argument("bootstack", type=Path)
    parser.add_argument("--output", type=Path, default=Path(".cache/qemu-uart"))
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    fixture = (ROOT / "tests/fixtures/stage2-el1.S").read_text()
    reports = []
    cases = [(f"mmu{mmu}-{name}", mmu, action, usb)
             for mmu in [0, 1] for name, action, usb in [
                 ("uart", 12, False), ("uart-usb", 12, True), ("owned-usb", 14, True),
                 ("uart-invalid-width", 15, True), ("wfi-physical-irq", 4, True)]]
    cases += [(f"uart-stage1-alias-usb{int(usb)}", 1, 13, usb) for usb in [False, True]]
    for nonce, (name, mmu, action, usb) in enumerate(cases, 1):
        raw = output / f"{name}.raw"
        payload.assemble(f".set SV_MMU,{mmu}\n.set SV_ACTION,{action}\n"
                         f".set SV_ADDRESS,0x700ff000\n.set SV_NONCE,{nonce}\n" + fixture, raw)
        expected = [4, 0 if action == 15 else 1, *([0] * 9), nonce]
        loaders = []
        physical = []
        fault = None
        if action in [12, 13]:
            expected[2:11] = [1, 0x60, 0xc1, 5, 0xffffffa5, 0xffffffffffffffa5, 0xa5, 0, 0]
        elif action == 4:
            expected[2:4] = [4, 30]
        elif action == 15:
            fault = (0x700ff000, 0x24, 7)
            loaders.append((0x700ff000, stage2.SENTINEL))
        else:
            for address in [0x70090000, 0x7009f000, 0x700d0000, 0x700d9180,
                            0x7d000000, 0x7d004000, 0x7d008000, 0x7d1ffff8]:
                loaders.append((address, stage2.SENTINEL))
                physical.append((address, stage2.SENTINEL))
            car = bytearray(4096)
            struct.pack_into("<I", car, 0x10, 0x40000021)
            struct.pack_into("<I", car, 0x610, 0xabcddcba)
            loaders.append((0x60006000, car))
            physical += [(0x60006010, struct.pack("<I", 0xffbfffff)),
                         (0x60006610, struct.pack("<I", 0xabcddcba)),
                         (0x60006330, struct.pack("<I", 0x7dffffff))]
            pmc = bytearray(4096)
            struct.pack_into("<I", pmc, 0x430, 0xdeadc0de)
            struct.pack_into("<I", pmc, 0x4f0, 12)
            loaders.append((0x7000e000, pmc))
            physical += [(0x7000e430, struct.pack("<I", 0x107)),
                         (0x7000e4f0, struct.pack("<I", 12))]
            loaders.append((0x7001928c, struct.pack("<I", 0xabcdef01)))
            physical.append((0x7001928c, struct.pack("<I", 0xabcdef01)))
            expected[2:9] = [0xffbfffff, 0xabcddcba, 0x7dffffff, 12, 0xdeadc0de, 0x107, 0]
        image = output / f"{name}.bin"
        packed = subprocess.run([str(ROOT / "target/debug/switchvisor-tool"), "pack-payload",
                                 str(args.bootstrap.resolve()), str(args.bootstack.resolve()),
                                 str(raw), "0x100000", str(image), "--entry-offset", "64",
                                 *(["--usb-uart"] if usb else [])], check=True, capture_output=True)
        (output / f"{name}-manifest.json").write_bytes(packed.stdout)
        report = payload.execute_guest(image, raw.read_bytes(), [], nonce, output / name,
                                       result_words=expected, fault=fault, extra_loaders=loaders,
                                       physical_words=physical)
        report.update({"case": name, "guest_mmu_enabled": bool(mmu), "usb_profile": usb,
                       "physical_usb_emulated": False})
        reports.append(report)
    (output / "verification.json").write_text(json.dumps(
        {"hardware_validated": False, "cases_passed": len(reports), "cases": reports}, indent=2) + "\n")


if __name__ == "__main__":
    main()
