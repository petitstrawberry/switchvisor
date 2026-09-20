#!/usr/bin/env python3
"""Exercise raw payload packaging guards against the real host CLI."""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bootstrap", type=Path)
    parser.add_argument("bootstack", type=Path)
    parser.add_argument("--output", type=Path, default=Path(".cache/payload-host.json"))
    args = parser.parse_args()
    bootstrap = args.bootstrap.resolve(strict=True)
    bootstack = args.bootstack.resolve(strict=True)
    tool = ROOT / "target/debug/switchvisor-tool"
    pins = {name: digest(bootstack / name) for name in ["bl31.bin", "bl33.bin", "nx-plat.dtimg"]}
    cases = []
    with tempfile.TemporaryDirectory(prefix="sv-raw-host-") as directory:
        temp = Path(directory)
        opaque = temp / "opaque.raw"
        opaque.write_bytes(bytes.fromhex("1f2003d51f2003d5"))
        empty = temp / "empty.raw"
        empty.write_bytes(b"")
        large = temp / "oversized.raw"
        with large.open("wb") as file:
            file.truncate(64 * 1024 * 1024 + 1)
        wrong_base = temp / "wrong-base.raw"
        data = bytearray(bootstrap.read_bytes())
        struct.pack_into("<Q", data, 8, 0xAA000000)
        wrong_base.write_bytes(data)
        populated = temp / "populated.raw"
        data = bytearray(bootstrap.read_bytes())
        data[4096] = 1
        populated.write_bytes(data)
        unaligned = temp / "unaligned.raw"
        data = bytearray(bootstrap.read_bytes()) + b"\0"
        struct.pack_into("<Q", data, 16, len(data))
        struct.pack_into("<Q", data, 24, len(data))
        unaligned.write_bytes(data)
        old_bootstrap = temp / "old-bootstrap.raw"
        data = bytearray(bootstrap.read_bytes())
        data[40:48] = b"SVBOOT04"
        old_bootstrap.write_bytes(data)

        def run(name, raw=bootstrap, payload=opaque, runtime="0x10000", options=(), succeeds=False, exists=False):
            output = temp / f"{name}.bin"
            if exists:
                output.write_bytes(b"preserve existing output")
                before = digest(output)
            result = subprocess.run([str(tool), "pack-payload", str(raw), str(bootstack),
                                     str(payload), runtime, str(output), *options], capture_output=True)
            assert (result.returncode == 0) == succeeds, (name, result.stderr.decode())
            if succeeds:
                manifest = json.loads(result.stdout)
                copied = output.read_bytes()[manifest["payload"]["offset"]:]
                assert copied == payload.read_bytes()
                assert manifest["payload"]["preserve_boot_args"]
                assert manifest["usb_control"]["packaged_payload_fallback"] == ("--no-fallback" not in options)
                assert manifest["usb_net"]["enabled"] == ("--usb-net" in options)
                detail = manifest
            else:
                assert digest(output) == before if exists else not output.exists(), name
                detail = result.stderr.decode().strip()
            cases.append({"name": name, "passed": True, "exit_code": result.returncode,
                          "output_preserved": exists, "result": detail})

        run("opaque-raw-accepted", succeeds=True)
        run("empty-raw-rejected", payload=empty)
        run("runtime-smaller-than-file", runtime="7")
        run("runtime-budget-exceeded", runtime="0x4000001")
        run("runtime-u64-overflow", runtime="18446744073709551616")
        run("entry-unaligned", options=["--entry-offset", "1"])
        run("entry-outside-file", options=["--entry-offset", "8"])
        run("duplicate-options", options=["--x0", "1", "--x0", "2"])
        run("unknown-options", options=["--format", "elf"])
        run("no-fallback-requires-usb", options=["--no-fallback"])
        run("no-fallback", options=["--usb-control", "--no-fallback"], succeeds=True)
        run("usb-net", options=["--usb-net"], succeeds=True)
        run("usb-net-with-console", options=["--usb-net", "--usb-uart"], succeeds=True)
        run("usb-net-no-fallback", options=["--usb-net", "--no-fallback"], succeeds=True)
        run("usb-net-duplicate", options=["--usb-net", "--usb-net"])
        run("old-bootstrap-without-net", raw=old_bootstrap, succeeds=True)
        run("old-bootstrap-with-net", raw=old_bootstrap, options=["--usb-net"])
        run("wrong-bootstrap-base", raw=wrong_base)
        run("populated-descriptor", raw=populated)
        run("unaligned-bootstrap", raw=unaligned)
        run("oversized-raw", payload=large)
        run("existing-output-preserved", exists=True)
    assert pins == {name: digest(bootstack / name) for name in pins}, "input bootstack changed"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps({"hardware_validated": False, "cases_passed": len(cases),
        "input_bootstack_unchanged": True, "bootstrap_sha256": digest(bootstrap),
        "bootstack_sha256": pins, "cases": cases}, indent=2) + "\n")
    print(f"Raw payload host CLI: {len(cases)} cases passed")


if __name__ == "__main__":
    main()
