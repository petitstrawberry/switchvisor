#!/usr/bin/env python3
"""Exercise native U-Boot memory discovery with the Rust MC policy and pinned ODIN DTB."""
import argparse
import importlib.util
import json
from pathlib import Path
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("payload_smoke", ROOT / "scripts/qemu-payload-smoke.py")
payload = importlib.util.module_from_spec(spec)
spec.loader.exec_module(payload)


def function(source, declaration):
    start = source.index(declaration)
    opening = source.index("{", start)
    depth = 1
    cursor = opening + 1
    while depth:
        if source[cursor] == "{": depth += 1
        if source[cursor] == "}": depth -= 1
        cursor += 1
    return source[start:cursor] + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="Native U-Boot checkout containing the pinned revision")
    parser.add_argument("bootstack", type=Path, help="Pinned native bootstack")
    args = parser.parse_args()
    source = args.source.resolve(strict=True)
    pins = ROOT / "crates/switchvisor-tool/config/bootstack.json"
    revision = json.loads(pins.read_text())["uboot_source_revision"]
    def pinned(path):
        return subprocess.check_output(["git", "-C", str(source), "show", f"{revision}:{path}"])
    subprocess.run([str(ROOT / "target/debug/switchvisor-tool"), "inspect-bootstack", str(args.bootstack)],
                   check=True, capture_output=True)
    image = (args.bootstack / "nx-plat.dtimg").read_bytes()
    magic, total, header, stride, count, entries, _, _ = struct.unpack_from(">8I", image)
    assert magic == 0xd7b7ab1e and total == len(image) and header >= 32 and stride >= 32
    # `dtimg load <image> 0 <destination>` selects entry zero (ODIN Erista).
    size, offset, board_id, revision_id = struct.unpack_from(">4I", image, entries)
    assert count == 4 and board_id == int.from_bytes(b"ODIN", "big") and revision_id == 0
    assert offset + size <= len(image)
    dtb = image[offset:offset + size]
    board = pinned("arch/arm/mach-tegra/board.c").decode()
    banks = pinned("arch/arm/mach-tegra/board2.c").decode()
    support = pinned("common/fdt_support.c").decode()
    arch = pinned("arch/arm/lib/bootm-fdt.c").decode()
    prefix = """
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <libfdt.h>
typedef uint64_t u64, phys_size_t, phys_addr_t;
typedef uint32_t u32;
typedef unsigned char u8;
typedef unsigned long ulong;
#define CONFIG_NR_DRAM_BANKS 3
#define MEMORY_BANKS_MAX 3
#define CONFIG_SYS_SDRAM_BASE 0x80000000ULL
#define CONFIG_PHYS_64BIT 1
#define CONFIG_ARM64 1
#define CONFIG_TEGRA210 1
#define CONFIG_TEGRA210_CARVEOUT_EXACT_SIZE 1
#define CONFIG_OF_LIBFDT 1
#define CONFIG_PCI 1
#define SZ_2G 0x80000000ULL
#define BIT(n) (1U << (n))
#define debug(...) ((void)0)
#include "mc.h"
static u32 mc_registers[1024];
#define NV_PA_MC_BASE ((uintptr_t)mc_registers)
extern u32 sv_mc_read(u64 offset, u32 physical);
extern int sv_mc_valid(const u32 *registers);
static bool virtualized;
static u32 readl(const u32 *address) {
    u64 offset = (uintptr_t)address - (uintptr_t)mc_registers;
    assert(offset < 4096 && !(offset & 3));
    return virtualized ? sv_mc_read(offset, *address) : *address;
}
struct bank { u64 start, size; };
typedef struct board_data { struct bank bi_dram[3]; } bd_t;
struct global_data { u64 ram_size, pci_ram_top; bd_t *bd; };
static struct global_data data, *gd = &data;
static bd_t bd;
"""
    functions = "".join(function(support, declaration) for declaration in [
        "int fdt_find_or_add_subnode(", "static int fdt_pack_reg(", "int fdt_fixup_memory_banks("])
    functions += function(board, "static phys_size_t query_sdram_size(")
    functions += "".join(function(banks, declaration) for declaration in [
        "phys_size_t carveout_t210_size(", "static ulong carveout_size(",
        "static ulong usable_ram_size_below_4g(", "static phys_size_t usable_ram_size_above_4g(",
        "int dram_init_banksize(", "ulong board_get_usable_ram_top("])
    functions += function(arch, "int arch_fixup_fdt(")
    with tempfile.TemporaryDirectory(prefix="sv-native-uboot-") as directory:
        temp = Path(directory)
        library = temp / "libfdt"; library.mkdir()
        paths = subprocess.check_output(["git", "-C", str(source), "ls-tree", "-r", "--name-only",
                                         revision, "scripts/dtc/libfdt"]).decode().splitlines()
        for path in paths: (library / Path(path).name).write_bytes(pinned(path))
        (temp / "mc.h").write_bytes(pinned("arch/arm/include/asm/arch-tegra210/mc.h"))
        (temp / "native.dtb").write_bytes(dtb)
        (temp / "mc.bin").write_bytes(payload.mc_fixture())
        c = temp / "native.c"
        c.write_text(prefix + functions + (ROOT / "tests/fixtures/uboot-memory.c").read_text())
        bridge = temp / "libmc_policy.a"
        subprocess.run(["rustc", "--edition=2024", "--crate-type=staticlib", "-C", "panic=abort",
            "-C", "opt-level=2", "--extern", f"switchvisor={ROOT / 'target/debug/libswitchvisor.rlib'}",
            str(ROOT / "tests/fixtures/mc-policy.rs"), "-o", str(bridge)], check=True)
        sources = [library / name for name in ["fdt.c", "fdt_ro.c", "fdt_rw.c", "fdt_wip.c",
            "fdt_sw.c", "fdt_strerror.c", "fdt_empty_tree.c", "fdt_addresses.c"]]
        binary = temp / "native"
        subprocess.run(["cc", "-std=gnu11", "-O2", "-I", str(library), "-I", str(temp), str(c),
                        *map(str, sources), str(bridge), "-o", str(binary)], check=True)
        subprocess.run([str(binary), str(temp / "native.dtb"), str(temp / "mc.bin")], check=True)
        print(f"Native U-Boot {revision}: Rust MC policy, relocation top, low/high banks, ODIN DTB: PASS")


if __name__ == "__main__": main()
