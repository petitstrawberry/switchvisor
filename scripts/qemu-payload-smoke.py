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
RUNTIME_SIZE = 0x100000

# Reuse the diagnostic's physical framebuffer decoder and linked WFE check.
spec = importlib.util.spec_from_file_location("boot_smoke", ROOT / "scripts/qemu-boot-smoke.py")
boot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(boot)
boot.GLYPHS.update({
    "P": [30, 17, 17, 30, 16, 16, 16],
    "R": [30, 17, 17, 30, 20, 18, 17],
})


def digest(data):
    return hashlib.sha256(data).hexdigest()


def assemble(source, output):
    obj = output.with_suffix(".o")
    subprocess.run(["llvm-mc", "-triple=aarch64", "-filetype=obj", "-o", str(obj)],
                   input=source.encode(), check=True)
    subprocess.run(["llvm-objcopy", "-O", "binary", str(obj), str(output)], check=True)


def markers(raw, rejected):
    checks = [(15, 0, "P"), (15, 8, "R")] if rejected else [
        (0, 0, "P"), (0, 8, "E"), (2, 0, "E"), (2, 2, "1"),
        *[(1, column, "0") for column in range(7, 23)],
    ]
    return all(boot.glyph_at(raw, row, col) == boot.GLYPHS[letter]
               for row, col, letter in checks)


def execute_guest(image, payload, registers, nonce, output, rejected=False):
    loaded = image.read_bytes()
    output.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix="sv-raw-qemu-") as directory:
        temp = Path(directory)
        entry = ".text\nmsr spsel, #0\n" + "".join(
            f"mov x{i}, #{0x100 + i}\n" for i in range(8)) + "mov x16, #0xaa000000\nbr x16\n"
        assemble(entry, temp / "entry.bin")
        (temp / "dirty.bin").write_bytes(b"\xff" * RESULT_SIZE)
        qmp = temp / "qmp.sock"
        command = ["qemu-system-aarch64", "-machine", "virt,virtualization=on,gic-version=2",
                   "-cpu", "cortex-a57", "-smp", "1", "-m", "3G", "-display", "none",
                   "-serial", "none", "-monitor", "none", "-S", "-qmp",
                   f"unix:{qmp},server=on,wait=off", "-device",
                   f"loader,file={image},addr=0xaa000000,force-raw=on", "-device",
                   f"loader,file={temp / 'dirty.bin'},addr={RESULT_BASE:#x},force-raw=on",
                   "-device", f"loader,file={temp / 'dirty.bin'},addr={STACK_TOP - RESULT_SIZE:#x},force-raw=on",
                   "-device", f"loader,file={temp / 'entry.bin'},addr=0x80000000,force-raw=on,cpu-num=0"]
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
                    client.connect(str(qmp))
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
                    initial = qmp_execute("human-monitor-command", {"command-line": "info registers"})
                    qmp_execute("cont")
                    while True:
                        time.sleep(0.2)
                        qmp_execute("stop")
                        raw = memory(boot.FB_BASE, boot.FB_SIZE, "framebuffer.raw")
                        final = qmp_execute("human-monitor-command", {"command-line": "info registers"})
                        if boot.park_offset(final, loaded) is not None and markers(raw, rejected):
                            break
                        if time.monotonic() > deadline:
                            boot.png(raw, output / "failure.png")
                            raise RuntimeError("Missing payload terminal markers:\n" + final)
                        qmp_execute("cont")
                    result = memory(RESULT_BASE, RESULT_SIZE, "result.raw")
                    copied = memory(0xAA000000, len(payload), "copied.raw")
                    stack = memory(STACK_TOP - RESULT_SIZE, RESULT_SIZE, "stack.raw")
                    qmp_execute("quit")
                process.wait(timeout=5)
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)
    boot.png(raw, output / "framebuffer.png")
    report = {
        "hardware_validated": False, "stage2_enabled": False, "qemu_cpu": "cortex-a57",
        "mode": "crc-rejection" if rejected else "el1-handoff",
        "image_sha256": digest(loaded), "payload_sha256": digest(payload),
        "cpu_park_verified": True, "resident_base": "0xb0000000",
        "framebuffer_sha256": digest(raw), "image": "framebuffer.png",
        "initial_registers": initial, "final_registers": final,
    }
    if rejected:
        assert result == b"\xff" * RESULT_SIZE, "rejected input changed runtime RAM"
        assert copied == loaded[:len(payload)], "rejected input overwrote the package"
        assert stack == b"\xff" * RESULT_SIZE, "rejected input cleared guest stack"
        report.update({"rejection_visible": True, "destination_prefix_unchanged": True,
                       "runtime_sample_unchanged": True, "stack_sample_unchanged": True})
    else:
        words = struct.unpack("<12Q", result)
        assert words == (4, STACK_TOP, *registers, 0x30D00800, nonce), words
        assert copied == payload, "opaque raw payload copy changed bytes"
        assert stack == bytes(RESULT_SIZE), "guest stack was not cleared"
        report.update({"current_el": "EL1", "entry_offset": 64,
                       "stack_top": hex(words[1]), "registers_x0_x7": list(words[2:10]),
                       "sctlr_el1": hex(words[10]), "nonce": words[11],
                       "runtime_tail_sample_zero_verified": True, "stack_sample_zero_verified": True,
                       "raw_copy_verified": True, "successful_hvc_exit_visible": True})
    (output / "verification.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"QEMU {output.name}: PASS", flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bootstrap", type=Path, help="Raw EL2 bootstrap linked at 0xB0000000")
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
            # Exercise a source/destination overlap as well as the small raw payload.
            data = payload.read_bytes()
            payload.write_bytes(data + b"\xa5" * (0x20000 - len(data)))
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
        "hardware_validated": False, "stage2_enabled": False, "cases": reports}, indent=2) + "\n")


if __name__ == "__main__":
    main()
