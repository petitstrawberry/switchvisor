#!/usr/bin/env python3
"""Execute the real diagnostic BL33 and inspect its framebuffer; no Switch hardware."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import socket
import struct
import subprocess
import tempfile
import time
import zlib

FB_BASE = 0xF5A00000
FB_SIZE = 720 * 1280 * 4
FG = 0xFFEEF2F6
GLYPHS = {
    "S": [15,16,16,14,1,1,30], "E": [31,16,16,30,16,16,31],
    "L": [16,16,16,16,16,16,31], "2": [14,17,1,2,4,8,31],
    "N": [17,25,21,19,17,17,17], "G": [14,17,16,23,17,17,15],
    "0": [14,17,19,21,25,17,14], "1": [4,12,4,4,4,4,14],
    "7": [31,1,2,4,8,8,8],
    "F": [31,16,16,30,16,16,16], "A": [14,17,17,31,17,17,17],
}

def glyph_at(raw, row, column):
    glyph = []
    for gy in range(7):
        bits = 0
        for gx in range(5):
            x = 16 + column * 12 + gx * 2
            y = 16 + row * 16 + gy * 2
            pixel = struct.unpack_from("<I", raw, (1279-x)*2880 + y*4)[0]
            bits = (bits << 1) | (pixel == FG)
        glyph.append(bits)
    return glyph

def visible_markers(raw):
    checks = [(0,0,"S"), (15,0,"E"), (15,1,"N"), (16,0,"G")]
    # The title is SWITCHVISOR; CurrentEL is printed on row 2.
    checks += [(2,12+i,c) for i,c in enumerate("EL2")]
    # Nonzero x0..x7 originate in the EL2t trampoline. Verify x0 and x7's 0x10x suffix.
    checks += [(row,18,"1") for row in [6,13]]
    checks += [(6,19,"0"), (6,20,"0"), (13,20,"7")]
    return all(glyph_at(raw,row,col) == GLYPHS[c] for row,col,c in checks)

def park_offset(registers, image):
    match = re.search(r"PC=([0-9a-fA-F]+)",registers)
    if not match: return None
    linked_base = struct.unpack_from("<Q",image,8)[0]
    offset = int(match.group(1),16)-linked_base
    for candidate in [offset,offset-4]:
        if 0<=candidate<=len(image)-4 and image[candidate:candidate+4]==struct.pack("<I",0xD503205F):
            return candidate
    return None

def exception_markers(raw):
    # FATAL ... title and ESR=f2000057 are produced by the actual EL2 exception handler.
    checks = [(0,0,"F"),(0,1,"A"),(1,14,"F"),(1,15,"2"),(1,21,"7")]
    return all(glyph_at(raw,row,col)==GLYPHS[c] for row,col,c in checks)

def png(raw, path):
    lines = bytearray()
    for y in range(720):
        lines.append(0)
        for x in range(1280):
            i = (1279-x)*2880 + y*4
            lines.extend((raw[i+2],raw[i+1],raw[i]))
    def chunk(kind, data):
        return struct.pack(">I",len(data))+kind+data+struct.pack(">I",zlib.crc32(kind+data)&0xFFFFFFFF)
    path.write_bytes(b"\x89PNG\r\n\x1a\n"+chunk(b"IHDR",struct.pack(">IIBBBBB",1280,720,8,2,0,0,0))+chunk(b"IDAT",zlib.compress(lines))+chunk(b"IEND",b""))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image",type=Path)
    parser.add_argument("--output",type=Path,default=Path(".cache/qemu-boot"))
    parser.add_argument("--inject-exception-from",type=Path,help="Use a successful normal-run report to replace its WFE with BRK in a temporary image")
    args = parser.parse_args()
    image = args.image.resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True,exist_ok=True)
    original = image.read_bytes()
    loaded = original
    injected_offset = None
    if args.inject_exception_from:
        previous = json.loads(args.inject_exception_from.read_text())
        assert previous["mode"]=="normal" and previous["cpu_park_verified"],"a successful normal-run report is required"
        assert previous["image_sha256"]==hashlib.sha256(original).hexdigest(),"normal-run report/image mismatch"
        injected_offset = park_offset(previous["final_registers"],original)
        assert injected_offset is not None,"normal run was not parked at WFE"
        loaded = bytearray(original)
        loaded[injected_offset:injected_offset+4] = struct.pack("<I",0xD4200000|(0x57<<5))
        loaded = bytes(loaded)
    with tempfile.TemporaryDirectory(prefix="sv-qemu-") as directory:
        temp = Path(directory)
        (temp/"loaded.bin").write_bytes(loaded)
        entry = ".text\nmsr spsel, #0\n" + "".join(f"mov x{i}, #{0x100+i}\n" for i in range(8)) + "mov x16, #0xaa000000\nbr x16\n"
        subprocess.run(["llvm-mc","-triple=aarch64","-filetype=obj","-o",str(temp/"entry.o")],input=entry.encode(),check=True)
        subprocess.run(["llvm-objcopy","-O","binary",str(temp/"entry.o"),str(temp/"entry.bin")],check=True)
        qmp = temp/"qmp.sock"
        command = ["qemu-system-aarch64","-machine","virt,virtualization=on,gic-version=2","-cpu","cortex-a57","-smp","1","-m","3G","-display","none","-serial","none","-monitor","none","-S","-qmp",f"unix:{qmp},server=on,wait=off","-device",f"loader,file={temp/'loaded.bin'},addr=0xaa000000,force-raw=on","-device",f"loader,file={temp/'entry.bin'},addr=0x80000000,force-raw=on,cpu-num=0"]
        with (output/"qemu.log").open("wb") as log:
            process = subprocess.Popen(command,stdout=log,stderr=log)
            try:
                deadline = time.monotonic()+15
                while not qmp.exists():
                    if process.poll() is not None: raise RuntimeError((output/"qemu.log").read_text())
                    if time.monotonic()>deadline: raise TimeoutError("QMP startup")
                    time.sleep(0.05)
                with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
                    client.settimeout(5)
                    client.connect(str(qmp))
                    stream = client.makefile("rwb")
                    json.loads(stream.readline())
                    def execute(name, arguments=None):
                        stream.write((json.dumps({"execute":name,"arguments":arguments or {}})+"\n").encode()); stream.flush()
                        while True:
                            response = json.loads(stream.readline())
                            if "error" in response: raise RuntimeError(response["error"])
                            if "return" in response: return response["return"]
                    execute("qmp_capabilities")
                    initial = execute("human-monitor-command",{"command-line":"info registers"})
                    execute("cont")
                    while True:
                        time.sleep(0.2)
                        execute("stop")
                        result = execute("human-monitor-command",{"command-line":f'pmemsave {FB_BASE:#x} {FB_SIZE:#x} "{temp / "framebuffer.raw"}"'})
                        if result: raise RuntimeError(result)
                        raw = (temp/"framebuffer.raw").read_bytes()
                        final = execute("human-monitor-command",{"command-line":"info registers"})
                        visible = exception_markers(raw) if args.inject_exception_from else visible_markers(raw)
                        if len(raw)==FB_SIZE and visible and park_offset(final,loaded) is not None: break
                        if time.monotonic()>deadline:
                            png(raw,output/"framebuffer.png")
                            registers = execute("human-monitor-command",{"command-line":"info registers"})
                            raise RuntimeError("Missing EL2/framebuffer/handoff/final markers:\n"+registers)
                        execute("cont")
                    execute("quit")
                process.wait(timeout=5)
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)
    png(raw,output/"framebuffer.png")
    report = {
        "hardware_validated":False,"qemu_cpu":"cortex-a57","entry":"EL2t via test trampoline",
        "image_sha256":hashlib.sha256(original).hexdigest(),
        "loaded_image_sha256":hashlib.sha256(loaded).hexdigest(),
        "mode":"exception-injection" if args.inject_exception_from else "normal",
        "injected_brk_offset":injected_offset,"cpu_park_verified":True,
        "framebuffer_sha256":hashlib.sha256(raw).hexdigest(),"image":"framebuffer.png",
        "initial_registers":initial,"final_registers":final,
    }
    if args.inject_exception_from:
        report.update({"fatal_exception_visible":True,"esr_brk_57_visible":True})
    else:
        report.update({"el2_log_visible":True,"nonzero_handoff_visible":True,"final_park_marker_visible":True})
    (output/"verification.json").write_text(json.dumps(report,indent=2)+"\n")
    print("QEMU "+report["mode"]+" framebuffer and CPU park: PASS")
    print(output/"framebuffer.png")

if __name__=="__main__": main()
