# switchvisor

A lightweight hypervisor for Nintendo Switch.

Switchvisor accepts an external raw AArch64 payload as BL33. The usual payload is U-Boot, which loads the guest OS, DTB, and initramfs through the existing boot scripts.

The boot path uses Hekate L4T and its existing BL31. Early boot messages use Hekate's framebuffer.

```text
Hekate -> BL31 -> Switchvisor (EL2) -> U-Boot (EL1) -> guest OS
```

## Build environment

`flake.nix` provides the Rust toolchain and development tools.
From the repository root, enter the Nix development shell:

```sh
nix develop
```

Run the following build and development commands inside this shell.

## Building a payload

Prepare a raw AArch64 binary and a bootstack directory containing `bl31.bin`, `bl33.bin`, and `nx-plat.dtimg`. The bootstack files must match [config/bootstack.json](config/bootstack.json); the packager uses them to verify the bootstack and extract the Hekate probe FDT. Supply the EL1 payload as a separate raw file.

```text
scripts/build-payload.sh <payload.raw> <runtime-size> [bootstack-directory] [output-directory] [payload options...]
```

For example:

```sh
scripts/build-payload.sh path/to/payload.raw 0x100000 path/to/bootstack .cache/payload
```

`runtime-size` is the full memory extent of the payload, including BSS. It must be at least the file size. Numeric arguments accept decimal or `0x` hexadecimal notation.

The script builds Switchvisor and prints the paths to `bl33.bin` and `manifest.json`. Their new `build.XXXXXX` directory contains:

| File | Contents |
|---|---|
| `bl33.bin` | Composite BL33 containing Switchvisor, the Hekate probe, and the raw payload |
| `manifest.json` | Payload entry, sizes, hashes, and bootstack pins |
| `bootstrap.raw` | Switchvisor bootstrap for packaging another payload |

The default bootstack directory is `../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/bootstack`. The default output directory is `.cache/payload`.

### Using U-Boot

The pinned native BL33 can be used directly as the external payload:

```sh
scripts/build-payload.sh path/to/bootstack/bl33.bin 0x68200 path/to/bootstack .cache/payload
```

The runtime size above applies to the pinned BL33 in `config/bootstack.json`. For another image, supply its own full runtime extent.

Use the existing microSD boot files and scripts. U-Boot discovers RAM through the virtualized MC registers, keeps its relocation and allocations below `0xFEC00000`, and writes the reduced memory banks into the OS DTB. No U-Boot source patch, control-DTB preparation, or additional boot-script reservation is required for this memory policy.

This path requires the Erista Hekate L4T memory layout with GSC5 disabled and usable low RAM through `0xFFC00000`. Switchvisor checks the physical MC geometry before copying itself to the resident region. Existing firmware carveouts and RAM above 4 GiB are preserved in U-Boot's memory discovery. The 2 MiB alignment leaves a 1 MiB gap before the default GPU firmware carveout, so the low memory bank loses 17 MiB in this profile.

Another bootloader must respect the VMM reservation in its own allocations and OS handoff. If it loads a standalone OS DTB, prepare that file with:

```sh
cargo run -p switchvisor-tool -- prepare-dtb path/to/input.dtb path/to/guest.dtb
```

This subtracts the VMM from memory banks and preserves existing firmware reservations. Use the resulting DTB in that bootloader's normal OS loading flow.

### Entry and register options

Pass payload options after both directory arguments:

```sh
scripts/build-payload.sh path/to/payload.raw 0x100000 path/to/bootstack .cache/payload \
  --entry-offset 0x40 --x0 0x42
```

`--entry-offset` defaults to zero. `--x0` through `--x7` set explicit entry registers. Supplying any register option selects explicit inputs for all eight registers; omitted registers become zero. With no register options, the original BL31 x0-x7 are preserved.

### Replacing the payload

Reuse `bootstrap.raw` from that build directory to package another raw file without rebuilding Switchvisor. Replace `build.XXXXXX` below with the directory from your build:

```sh
target/debug/switchvisor-tool pack-payload \
  .cache/payload/build.XXXXXX/bootstrap.raw path/to/bootstack \
  path/to/another.raw 0x100000 .cache/another-bl33.bin
```

The same entry and register options can be appended to this command. The output file must be new.

## Payload entry contract

Supply a raw binary that can execute at the fixed BL33 load address. Its file bytes are copied unchanged, and the remaining memory through `runtime-size` is cleared.

| Item | Value |
|---|---|
| Load address | `0xAA000000` |
| Entry address | Load address plus `--entry-offset`; 4-byte aligned and inside the file |
| CPU state | AArch64 EL1h, DAIF masked, MMU and caches off |
| Guest address space | GPA=HPA, 36-bit Stage-2; VMM RAM `0xFEC00000`–`0xFFC00000` is unmapped |
| Memory discovery | MC page `0x70019000` is trapped; disabled GSC5 advertises the VMM reservation |
| Interrupts | Physical IRQ/FIQ/SError delivered directly to EL1 |
| Firmware calls | Native SMC forwarding; PSCI CPU_ON and suspend are unsupported |
| x0-x7 | Original BL31 inputs, or explicit register options |
| x8-x30 | Zero |
| Initial stack | SP_EL1=`0x8A800000`; the preceding 64 KiB are cleared |
| Size limits | Composite package and payload runtime each at most 64 MiB |

Package overhead reduces the maximum raw file size. The Hekate environment window preceding `0xAA000000` is preserved. Payloads must keep any required pointed-to data outside the composite package that is overwritten during installation.

MC emulation supports aligned 32-bit loads/stores with valid AArch64 abort syndrome information, including signed loads. The virtual GSC5 address and size registers are read-only; other accesses in the MC page pass through to hardware. Unsupported MC accesses stop the payload.

Run payloads on CPU0. Keep the VMM region out of bootloader allocations, OS memory banks, and device DMA buffers. A CPU access to this region faults at EL2 and stops the payload. Stage-2 does not constrain device DMA. The fixed VMM placement requires usable RAM throughout that region on the target boot configuration.

## Development

```sh
cargo test --workspace
cargo check-el2
cargo build-boot
```
