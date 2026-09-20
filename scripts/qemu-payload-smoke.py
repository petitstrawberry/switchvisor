#!/usr/bin/env python3
"""Pack external raw BL33 files and execute the CPU0 EL1 handoff on QEMU A57."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
RESULT_BASE = 0xAA080000
RESULT_SIZE = 96
STACK_TOP = 0x8A800000
RESIDENT_BASE = 0xFEC00000
RESIDENT_SIZE = 0x1000000
MC_BASE = 0x70019000
RUNTIME_SIZE = 0x100000

# Share the physical framebuffer decoder and linked WFE check across scenarios.
spec = importlib.util.spec_from_file_location("qemu_support", ROOT / "scripts/qemu_support.py")
qemu = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qemu)
qemu.GLYPHS.update({
    "P": [30, 17, 17, 30, 16, 16, 16],
    "R": [30, 17, 17, 30, 20, 18, 17],
})


def digest(data):
    return hashlib.sha256(data).hexdigest()


def mc_fixture():
    fixture = json.loads((ROOT / "tests/fixtures/mc-registers.json").read_text())
    data = bytearray(4096)
    for offset, value in fixture["words"].items():
        struct.pack_into("<I", data, int(offset, 0), int(value, 0))
    return data


def assemble(source, output):
    obj = output.with_suffix(".o")
    subprocess.run(["llvm-mc", "-triple=aarch64", "-filetype=obj", "-o", str(obj)],
                   input=source.encode(), check=True)
    subprocess.run(["llvm-objcopy", "-O", "binary", str(obj), str(output)], check=True)


def markers(raw, rejected, fault=False):
    checks = [(0, 0, "F"), (0, 1, "A")] if fault else [(15, 0, "P"), (15, 8, "R")] if rejected else [
        (0, 0, "P"), (0, 8, "E"), (2, 0, "E"), (2, 2, "1"),
        *[(1, column, "0") for column in range(7, 23)],
    ]
    return all(qemu.glyph_at(raw, row, col) == qemu.GLYPHS[letter]
               for row, col, letter in checks)


def execute_guest(image, payload, registers, nonce, output, rejected=False, *,
                  fault=None, result_words=None, extra_loaders=(), secure=False, entry_source=None,
                  placement_rejected=False, physical_words=(), cpu_count=1, firmware=False, inspect_cpu=0, diagnostic_regions=()):
    loaded = image.read_bytes()
    output.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="sv-raw-qemu-") as directory:
        temp = Path(directory)
        entry = ".text\nmsr spsel, #0\n" + "".join(
            f"mov x{i}, #{0x100 + i}\n" for i in range(8)) + "mov x16, #0xaa000000\nbr x16\n"
        assemble(entry_source or entry, temp / "entry.bin")
        (temp / "dirty.bin").write_bytes(b"\xff" * RESULT_SIZE)
        mc = mc_fixture()
        for address, data in extra_loaders:
            if MC_BASE <= address < MC_BASE + len(mc):
                offset = address - MC_BASE
                assert offset + len(data) <= len(mc)
                mc[offset:offset + len(data)] = data
        (temp / "mc.bin").write_bytes(mc)
        qmp = temp / "qmp.sock"
        command = ["qemu-system-aarch64", "-machine", "virt,virtualization=on,gic-version=2" + (",secure=on" if secure else ""),
                   "-cpu", "cortex-a57", "-smp", str(cpu_count), "-m", "3G", "-display", "none",
                   "-serial", "none", "-monitor", "none", "-S", "-qmp",
                   f"unix:{qmp},server=on,wait=off", "-device",
                   f"loader,file={image},addr=0xaa000000,force-raw=on", "-device",
                   f"loader,file={temp / 'dirty.bin'},addr={RESULT_BASE:#x},force-raw=on",
                   "-device", f"loader,file={temp / 'dirty.bin'},addr={STACK_TOP - RESULT_SIZE:#x},force-raw=on",
                   "-device", f"loader,file={temp / 'mc.bin'},addr={MC_BASE:#x},force-raw=on"]
        if firmware:
            command.extend(["-bios",str(temp / "entry.bin")])
        else:
            command.extend(["-device",f"loader,file={temp / 'entry.bin'},addr=0x80000000,force-raw=on,cpu-num=0"])
        for address, data in extra_loaders:
            if MC_BASE <= address < MC_BASE + len(mc):
                continue
            path = temp / f"extra-{address:x}.bin"
            path.write_bytes(data)
            command.extend(["-device", f"loader,file={path},addr={address:#x},force-raw=on"])
        with (output / "qemu.log").open("wb") as log:
            process = subprocess.Popen(command, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 20
                while not qmp.exists():
                    if process.poll() is not None:
                        raise RuntimeError((output / "qemu.log").read_text())
                    if time.monotonic() > deadline:
                        raise TimeoutError("QMP startup")
                    time.sleep(0.05)
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.settimeout(5)
                    while True:
                        try:
                            client.connect(str(qmp))
                            break
                        except ConnectionRefusedError:
                            if process.poll() is not None or time.monotonic() > deadline:
                                raise RuntimeError("QMP did not start: " + (output / "qemu.log").read_text())
                            time.sleep(0.05)
                    stream = client.makefile("rwb")
                    json.loads(stream.readline())

                    def qmp_execute(name, arguments=None):
                        stream.write((json.dumps({"execute": name, "arguments": arguments or {}})
                                      + "\n").encode())
                        stream.flush()
                        while True:
                            response = json.loads(stream.readline())
                            if "error" in response:
                                raise RuntimeError(response["error"])
                            if "return" in response:
                                return response["return"]

                    def memory(address, size, name):
                        path = temp / name
                        message = qmp_execute("human-monitor-command", {
                            "command-line": f'pmemsave {address:#x} {size:#x} "{path}"'})
                        if message:
                            raise RuntimeError(message)
                        return path.read_bytes()

                    qmp_execute("qmp_capabilities")
                    initial = qmp_execute("human-monitor-command", {"command-line": "info registers", "cpu-index":inspect_cpu})
                    qmp_execute("cont")
                    while True:
                        time.sleep(0.2)
                        qmp_execute("stop")
                        raw = memory(qemu.FB_BASE, qemu.FB_SIZE, "framebuffer.raw")
                        final = qmp_execute("human-monitor-command", {"command-line": "info registers", "cpu-index":inspect_cpu})
                        park_image = bytearray(loaded)
                        if placement_rejected:
                            struct.pack_into("<Q", park_image, 8, 0xaa000000)
                        if qemu.park_offset(final, park_image) is not None and (placement_rejected or markers(raw, rejected, fault is not None)):
                            break
                        if time.monotonic() > deadline:
                            diagnostics = []
                            for cpu in range(cpu_count):
                                diagnostics.append(qmp_execute("human-monitor-command", {"command-line":"info registers", "cpu-index":cpu}))
                            (output / "failure-registers.txt").write_text("\n".join(diagnostics))
                            if cpu_count > 1:
                                (output / "failure-cpu-results.raw").write_bytes(memory(0xaa081000,256,"failure-cpus.raw"))
                                (output / "failure-firmware.raw").write_bytes(memory(0x80030000,256,"failure-fw.raw"))
                            (output / "failure-result.raw").write_bytes(memory(RESULT_BASE, RESULT_SIZE, "failure-result.raw"))
                            for address, length in diagnostic_regions:
                                name = f"diagnostic-{address:x}.raw"
                                (output / name).write_bytes(memory(address, length, name))
                            qemu.png(raw, output / "failure.png")
                            raise RuntimeError("Missing payload terminal markers:\n" + final)
                        qmp_execute("cont")
                    result = memory(RESULT_BASE, RESULT_SIZE, "result.raw")
                    copied = memory(0xAA000000, len(payload), "copied.raw")
                    stack = memory(STACK_TOP - RESULT_SIZE, RESULT_SIZE, "stack.raw")
                    protected = memory(fault[0], 8, "protected.raw") if fault else None
                    physical = [(address, memory(address, len(expected), f"physical-{address:x}.raw"), expected)
                                for address, expected in physical_words]
                    cpu_registers = []
                    for cpu in range(cpu_count):
                        cpu_registers.append(qmp_execute("human-monitor-command", {"command-line":"info registers", "cpu-index":cpu}))
                    qmp_execute("quit")
                process.wait(timeout=5)
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)
    qemu.png(raw, output / "framebuffer.png")
    report = {
        "hardware_validated": False, "stage2_enabled": not rejected, "qemu_cpu": "cortex-a57",
        "mode": "crc-rejection" if rejected else "el1-handoff",
        "image_sha256": digest(loaded), "payload_sha256": digest(payload),
        "cpu_park_verified": True, "resident_base": hex(RESIDENT_BASE),
        "framebuffer_sha256": digest(raw), "image": "framebuffer.png",
        "initial_registers": initial, "final_registers": final,
        "cpu_count":cpu_count, "inspected_cpu":inspect_cpu, "all_cpu_registers":cpu_registers,
    }
    for address, actual, expected in physical:
        assert actual == expected, (hex(address), actual.hex(), expected.hex())
    if physical:
        report["physical_memory_verified"] = [hex(address) for address, _, _ in physical]
    if rejected or placement_rejected:
        assert result == b"\xff" * RESULT_SIZE, "rejected input changed runtime RAM"
        assert copied == loaded[:len(payload)], "rejected input overwrote the package"
        assert stack == b"\xff" * RESULT_SIZE, "rejected input cleared guest stack"
        report.update({"rejection_visible": True, "destination_prefix_unchanged": True,
                       "runtime_sample_unchanged": True, "stack_sample_unchanged": True})
        if placement_rejected:
            report.update({"mode":"placement-rejection", "stage2_enabled":False, "rejection_visible":False})
    else:
        words = struct.unpack("<12Q", result)
        assert words == (tuple(result_words) if result_words is not None else
                         (4, STACK_TOP, *registers, 0x30D00800, nonce)), words
        assert copied == payload, "opaque raw payload copy changed bytes"
        assert stack == bytes(RESULT_SIZE), "guest stack was not cleared"
        if result_words is None:
            report.update({"current_el": "EL1", "entry_offset": 64,
                       "stack_top": hex(words[1]), "registers_x0_x7": list(words[2:10]),
                       "sctlr_el1": hex(words[10]), "nonce": words[11],
                       "runtime_tail_sample_zero_verified": True, "stack_sample_zero_verified": True,
                       "raw_copy_verified": True, "successful_hvc_exit_visible": True})
        else:
            report.update({"mode":"stage2-fault" if fault else "stage2-guest", "result_words":list(words),
                           "raw_copy_verified":True, "protected_sample":protected.hex() if protected else None})
        if fault:
            def hex_line(row, column):
                reverse = {tuple(value):key for key,value in qemu.GLYPHS.items() if key in "0123456789ABCDEF"}
                return int("".join(reverse[tuple(qemu.glyph_at(raw,row,column+i))] for i in range(16)),16)
            esr, far, hpfar = hex_line(1,6), hex_line(2,6), hex_line(3,8)
            assert esr >> 26 == fault[1] and esr & 0x3f == (fault[2] if len(fault)>2 else 6), hex(esr)
            assert far == fault[0] and hpfar == (fault[0] >> 8) & ~0xf, (hex(far),hex(hpfar))
            expected = loaded[:8] if fault[0] == RESIDENT_BASE else bytes.fromhex("f0debc9a78563412")
            assert protected == expected, "guest overwrote protected resident RAM"
            report.update({"esr_el2":hex(esr),"far_el2":hex(far),"hpfar_el2":hex(hpfar),
                           "protected_sample_unchanged":True})
    (output / "verification.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"QEMU {output.name}: PASS", flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bootstrap", type=Path, help="Raw EL2 bootstrap linked at 0xFEC00000")
    parser.add_argument("bootstack", type=Path, help="Pinned bootstack used for the Hekate probe")
    parser.add_argument("--output", type=Path, default=Path(".cache/qemu-payload"))
    args = parser.parse_args()
    bootstrap = args.bootstrap.resolve(strict=True)
    bootstack = args.bootstack.resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    tool = ROOT / "target/debug/switchvisor-tool"
    fixture = (ROOT / "tests/fixtures/raw-el1.S").read_text()
    reports = []
    for name, nonce, options, registers in [
        ("preserved", 0x1111, [], list(range(0x100, 0x108))),
        ("injected", 0x2222, ["--x0", "0x42", "--x7", "0x77"], [0x42, 0, 0, 0, 0, 0, 0, 0x77]),
    ]:
        payload = output / f"{name}.raw"
        assemble(f".set SV_NONCE, {nonce}\n" + fixture, payload)
        if name == "injected":
            # Exercise an overlapping source/destination copy within the runtime limit.
            data = payload.read_bytes()
            payload.write_bytes(data + b"\xa5" * (0x40000 - len(data)))
        image = output / f"{name}.bin"
        result = subprocess.run([str(tool), "pack-payload", str(bootstrap), str(bootstack),
                                 str(payload), hex(RUNTIME_SIZE), str(image),
                                 "--entry-offset", "64", *options], check=True, capture_output=True)
        (output / f"{name}-manifest.json").write_bytes(result.stdout)
        manifest = json.loads(result.stdout)
        if name == "injected":
            assert manifest["payload"]["offset"] < payload.stat().st_size, "copy did not overlap"
        reports.append(execute_guest(image, payload.read_bytes(), registers, nonce, output / name))
    corrupt = bytearray((output / "preserved.bin").read_bytes())
    corrupt[-1] ^= 1
    image = output / "corrupt.bin"
    image.write_bytes(corrupt)
    reports.append(execute_guest(image, (output / "preserved.raw").read_bytes(), [], 0,
                                 output / "rejected", rejected=True))
    assert reports[0]["payload_sha256"] != reports[1]["payload_sha256"]
    (output / "verification.json").write_text(json.dumps({"cases_passed": len(reports),
        "hardware_validated": False, "cases": reports}, indent=2) + "\n")


if __name__ == "__main__":
    main()
