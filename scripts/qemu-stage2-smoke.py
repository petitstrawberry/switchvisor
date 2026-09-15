#!/usr/bin/env python3
"""Exercise resident protection with guest MMU off/on and virtualized EL1 IRQs."""
import argparse
import importlib.util
import json
from pathlib import Path
import struct
import subprocess

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("payload_smoke", ROOT / "scripts/qemu-payload-smoke.py")
payload = importlib.util.module_from_spec(spec)
spec.loader.exec_module(payload)
payload.boot.GLYPHS.update({
    "3":[30,1,1,14,1,1,30], "4":[2,6,10,18,31,2,2], "5":[31,16,16,30,1,1,30],
    "6":[14,16,16,30,17,17,14], "8":[14,17,17,14,17,17,14], "9":[14,17,17,15,1,1,14],
    "B":[30,17,17,30,17,17,30], "C":[14,17,16,16,16,17,14], "D":[30,17,17,17,17,17,30],
})
SENTINEL = struct.pack("<Q",0x123456789abcdef0)


def dtb_fixture():
    strings = b"#address-cells\0#size-cells\0device_type\0reg\0"
    structure = bytearray()
    def word(value): structure.extend(struct.pack(">I",value))
    def begin(name):
        word(1); structure.extend(name.encode()+b"\0")
        while len(structure)%4: structure.append(0)
    def prop(offset,data):
        word(3); word(len(data)); word(offset); structure.extend(data)
        while len(structure)%4: structure.append(0)
    begin("")
    prop(0,struct.pack(">I",2)); prop(15,struct.pack(">I",2))
    begin("memory@80000000")
    prop(27,b"memory\0"); prop(39,struct.pack(">QQ",0x80000000,0x80000000))
    word(2); word(2); word(9)
    total=56+len(structure)+len(strings)
    return struct.pack(">10I",0xd00dfeed,total,56,56+len(structure),40,17,16,0,len(strings),len(structure))+bytes(16)+structure+strings


def property_offset(dtb, name):
    _,_,cursor,strings=struct.unpack_from(">4I",dtb)
    while True:
        token=struct.unpack_from(">I",dtb,cursor)[0]; cursor+=4
        if token==1: cursor=(dtb.index(0,cursor)+4)&~3
        elif token==3:
            size,offset=struct.unpack_from(">2I",dtb,cursor); cursor+=8
            end=dtb.index(0,strings+offset)
            if dtb[strings+offset:end].decode()==name: return cursor
            cursor=(cursor+size+3)&~3
        elif token==9: raise ValueError(name)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bootstrap",type=Path)
    parser.add_argument("bootstack",type=Path)
    parser.add_argument("--output",type=Path,default=Path(".cache/qemu-stage2"))
    parser.add_argument("--case", action="append", default=[], help="Run only a named case; repeat to select more")
    args=parser.parse_args()
    output=args.output.resolve(); output.mkdir(parents=True,exist_ok=False)
    fixture=(ROOT/"tests/fixtures/stage2-el1.S").read_text()
    tool=ROOT/"target/debug/switchvisor-tool"
    reports=[]
    cases=[]
    for mmu in [0,1]:
        for address in [0xfec00000,0xfec80000,0xffbffff8]:
            for action in [1,2]: cases.append((f"mmu{mmu}-{'read' if action==1 else 'write'}-{address:x}",mmu,action,address))
        cases.append((f"mmu{mmu}-execute",mmu,3,0xfec80000))
        for address in [0xfebffff8,0xffc00000]: cases.append((f"mmu{mmu}-identity-{address:x}",mmu,0,address))
        cases.append((f"mmu{mmu}-physical-irq",mmu,4,0))
        cases.append((f"mmu{mmu}-psci-guard",mmu,5,0))
        cases.append((f"mmu{mmu}-native-smc",mmu,7,0))
        cases.append((f"mmu{mmu}-mc-discovery",mmu,8,0))
        cases.append((f"mmu{mmu}-mc-passthrough",mmu,9,0))
        cases.append((f"mmu{mmu}-mc-unsupported",mmu,11,0x70019100))
    cases.append(("mc-stage1-alias",1,10,0))
    cases.append(("guest-dtb",0,6,0))
    guard_cases=[("ram-too-small",0x50,1024), ("firmware-overlap",0xc5c,0xfec00000), ("gsc5-in-use",0xd54,1)]
    unknown=set(args.case)-{case[0] for case in cases+guard_cases}
    if unknown: parser.error(f"Unknown cases: {sorted(unknown)}")
    for nonce,(name,mmu,action,address) in enumerate(cases,1):
        if args.case and name not in args.case: continue
        raw=output/f"{name}.raw"
        payload.assemble(f".set SV_MMU,{mmu}\n.set SV_ACTION,{action}\n.set SV_ADDRESS,{address}\n.set SV_NONCE,{nonce}\n"+fixture,raw)
        expected=[4,0 if action in [1,2,3,11] else 1,*([0]*9),nonce]
        loaders=[]; options=[]
        fault=None; physical=[]
        if action in [1,2,3]:
            fault=(address,0x20 if action==3 else 0x24)
            if address!=0xfec00000: loaders.append((address,SENTINEL))
        if action==11:
            fault=(address,0x24,7); loaders.append((address,SENTINEL))
        if action==8:
            expected[2:6]=[4096,0xfec00000,0,128]
            physical=[(payload.MC_BASE+0xd4c,bytes(12))]
        if action==9:
            expected[2:6]=[0xfffffffffeedface,0xfeedface,0xfeedface,0]
            physical=[(payload.MC_BASE+0x100,struct.pack("<3I",0xfeedface,0,0xfeedface))]
        if action==10: expected[2]=0xfec00000
        if action==0: expected[2]=0x1122334455667788
        if action==4: expected[2:4]=[4,30]
        if action==7: expected[2:6]=[0x10001,0x1234,0x5678,0x9abc]
        if action==6:
            original=output/"original.dtb"; original.write_bytes(dtb_fixture())
            prepared=output/"guest.dtb"
            subprocess.run([str(tool),"prepare-dtb",str(original),str(prepared)],check=True,capture_output=True)
            dtb=prepared.read_bytes(); reg=property_offset(dtb,"reg"); resident=property_offset(dtb,"switchvisor,resident-region")
            assert struct.unpack_from(">4Q",dtb,reg)==(0x80000000,0x7ec00000,0xffc00000,0x400000)
            loaders.append((0x8d000000,dtb))
            options=["--x0","0x8d000000","--x1",str(reg),"--x2",str(resident)]
            expected[2:8]=[0x80000000,0x7ec00000,0xffc00000,0x400000,0xfec00000,0x1000000]
        image=output/f"{name}.bin"
        result=subprocess.run([str(tool),"pack-payload",str(args.bootstrap.resolve()),str(args.bootstack.resolve()),
            str(raw),"0x100000",str(image),"--entry-offset","64",*options],check=True,capture_output=True)
        (output/f"{name}-manifest.json").write_bytes(result.stdout)
        report=payload.execute_guest(image,raw.read_bytes(),[],nonce,output/name,
            fault=fault,result_words=expected,extra_loaders=loaders,physical_words=physical,secure=action==7,
            entry_source=(ROOT/"tests/fixtures/smccc-el3.S").read_text() if action==7 else None)
        report.update({"case":name,"guest_mmu_enabled":bool(mmu),"action":action})
        reports.append(report)
    # A harmless guest supplies a package even when selecting only guard cases.
    if not args.case or any(name in args.case for name,_,_ in guard_cases):
        raw=output/"placement.raw"
        payload.assemble(".set SV_MMU,0\n.set SV_ACTION,0\n.set SV_ADDRESS,0x90000000\n.set SV_NONCE,0\n"+fixture,raw)
        image=output/"placement.bin"
        subprocess.run([str(tool),"pack-payload",str(args.bootstrap.resolve()),str(args.bootstack.resolve()),
            str(raw),"0x100000",str(image),"--entry-offset","64"],check=True,capture_output=True)
    for name,offset,value in guard_cases:
        if args.case and name not in args.case: continue
        mc=payload.mc_fixture(); struct.pack_into("<I",mc,offset,value)
        report=payload.execute_guest(image,raw.read_bytes(),[],0,output/name,
            placement_rejected=True,extra_loaders=[(payload.MC_BASE,mc),(payload.RESIDENT_BASE,SENTINEL)],
            physical_words=[(payload.RESIDENT_BASE,SENTINEL)])
        report["case"]=name; reports.append(report)
    (output/"verification.json").write_text(json.dumps({"hardware_validated":False,
        "cases_passed":len(reports),"cases":reports},indent=2)+"\n")


if __name__=="__main__": main()
