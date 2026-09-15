#!/usr/bin/env python3
"""Reject FP/SIMD and atomic RMW instructions in the linked cache-off EL2 image."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("elf",type=Path)
args = parser.parse_args()
assembly = subprocess.check_output(["llvm-objdump","-d",str(args.elf)],text=True)
instructions = 0
for line in assembly.splitlines():
    if not re.match(r"^\s*[0-9a-f]+:",line): continue
    fields = line.split("\t")
    if len(fields)<2: continue
    opcode = fields[1].strip()
    operands = fields[2].split("<",1)[0] if len(fields)>2 else ""
    instructions += 1
    if re.fullmatch(r"(?:ld(?:a)?x(?:r|p)|st(?:l)?x(?:r|p))[bh]?",opcode) or re.fullmatch(r"(?:casp?|swp|ldadd|ldclr|ldeor|ldset|ldsmax|ldsmin|ldumax|ldumin|stadd|stclr|steor|stset|stsmax|stsmin|stumax|stumin)[albh]*",opcode):
        raise SystemExit("Atomic RMW instruction in cache-off EL2: "+line.strip())
    if opcode.startswith("f") or opcode in ["ld1","ld2","ld3","ld4","st1","st2","st3","st4"] or re.search(r"\b(?:v[0-9]+(?:\.[0-9]+[bhsd])?|[bhsdq][0-9]+)\b",operands):
        raise SystemExit("FP/SIMD instruction: "+line.strip())
assert instructions>0,"no disassembly found"
print(json.dumps({"elf_sha256":hashlib.sha256(args.elf.read_bytes()).hexdigest(),"instructions_scanned":instructions,"fp_simd_instructions":0,"atomic_rmw_instructions":0,"passed":True},indent=2))
