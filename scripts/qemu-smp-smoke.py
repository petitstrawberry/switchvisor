#!/usr/bin/env python3
"""Exercise four EL1 CPUs through the production EL2 power/translation paths."""
import argparse
import importlib.util
import json
from pathlib import Path
import struct
import subprocess

ROOT=Path(__file__).resolve().parent.parent
spec=importlib.util.spec_from_file_location('stage2_smoke',ROOT/'scripts/qemu-stage2-smoke.py')
stage2=importlib.util.module_from_spec(spec);spec.loader.exec_module(stage2)
payload=stage2.payload

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('bootstrap',type=Path)
    parser.add_argument('bootstack',type=Path)
    parser.add_argument('--output',type=Path,default=Path('.cache/qemu-smp'))
    parser.add_argument('--usb-uart',action='store_true',help='Also exercise USB MMIO ownership with native guest WFI')
    args=parser.parse_args()
    output=args.output.resolve();output.mkdir(parents=True,exist_ok=False)
    reports=[]
    for index,(cpu,mmu) in enumerate([(0,0),(1,0),(2,0),(3,0),(0,1)]):
        name=('four-cpus-mmu1' if mmu else 'four-cpus') if cpu==0 else f'cpu{cpu}-resident-fault'
        nonce=0x100+index
        raw=output/f'{name}.raw'
        payload.assemble(f'.set SV_FAULT,{cpu}\n.set SV_MMU,{mmu}\n.set SV_NONCE,{nonce}\n'+(ROOT/'tests/fixtures/smp-el1.S').read_text(),raw)
        image=output/f'{name}.bin'
        result=subprocess.run([str(ROOT/'target/debug/switchvisor-tool'),'pack-payload',str(args.bootstrap.resolve()),
            str(args.bootstack.resolve()),str(raw),'0x100000',str(image),'--entry-offset','64',
            *(['--usb-uart'] if args.usb_uart else [])],check=True,capture_output=True)
        (output/f'{name}-manifest.json').write_bytes(result.stdout)
        expected=[4,15 if cpu==0 else 0,1 if cpu==0 else 0,*([0]*8),nonce]
        physical=[(payload.MC_BASE+0xd4c,bytes(12))]
        if cpu==0:
            for target in range(1,4):
                context=0xcafe0000+target
                if target!=2:context|=0xfedcba98<<32
                physical.append((0xaa081000+target*64,struct.pack('<6Q',4,context,0xfec00000,1,2 if target < 3 else 1,1)))
                physical.append((0x80030000+target*64+16,struct.pack('<Q',1)))
            physical.append((0xaa081000+3*64+48,struct.pack('<Q',1)))
        report=payload.execute_guest(image,raw.read_bytes(),[],nonce,output/name,
            result_words=expected,secure=True,firmware=True,cpu_count=4,inspect_cpu=cpu,
            entry_source=(ROOT/'tests/fixtures/smp-el3.S').read_text(),physical_words=physical,
            fault=(payload.RESIDENT_BASE,0x24) if cpu else None)
        report['case']=name;report['usb_profile']=args.usb_uart;reports.append(report)
    (output/'verification.json').write_text(json.dumps({'hardware_validated':False,'cases_passed':len(reports),'cases':reports},indent=2)+'\n')

if __name__=='__main__':main()
