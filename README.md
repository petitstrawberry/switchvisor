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
scripts/build-payload.sh path/to/payload.raw 0x100000 path/to/bootstack
```

`runtime-size` is the full memory extent of the payload, including BSS. It must be at least the file size. Numeric arguments accept decimal or `0x` hexadecimal notation.

The script builds Switchvisor and writes these files to `dist/`:

| File | Contents |
|---|---|
| `bl33.bin` | Composite BL33 containing Switchvisor, the Hekate probe, and the raw payload |
| `manifest.json` | Payload entry, sizes, hashes, and bootstack pins |
| `bootstrap.raw` | Switchvisor bootstrap for packaging another payload |

The default bootstack directory is `../scarlet-project-switch/projects/aarch64-switch-console/.scarlet/bootstack`. An optional output-directory argument overrides `dist/`. The three output files are replaced after the build and packaging succeed.

Copy `dist/bl33.bin` to the microSD path configured for BL33 in your Hekate L4T entry. For the Scarlet Switch Console entry, replace `/switchroot/scarlet-console/bl33.bin`.

### Using U-Boot

The pinned native BL33 can be used directly as the external payload:

```sh
scripts/build-payload.sh path/to/bootstack/bl33.bin 0x68200 path/to/bootstack
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
scripts/build-payload.sh path/to/payload.raw 0x100000 path/to/bootstack dist \
  --entry-offset 0x40 --x0 0x42
```

`--entry-offset` defaults to zero. `--x0` through `--x7` set explicit entry registers. Supplying any register option selects explicit inputs for all eight registers; omitted registers become zero. With no register options, the original BL31 x0-x7 are preserved.

### Replacing the payload

Reuse `dist/bootstrap.raw` to package another raw file without rebuilding Switchvisor:

```sh
target/debug/switchvisor-tool pack-payload \
  dist/bootstrap.raw path/to/bootstack \
  path/to/another.raw 0x100000 dist/another-bl33.bin
```

The same entry and register options can be appended to this command. The output file must be new.

## USB console

Enable the EL2-owned USB 2.0 CDC ACM transport when packaging:

```sh
scripts/build-payload.sh path/to/bootstack/bl33.bin 0x68200 path/to/bootstack dist --usb-uart
```

Copy `dist/bl33.bin` to your BL33 path. This build also produces `dist/usb-uart.dtbo`, an overlay for the pinned Noble ODIN platform DTB. It adds a bidirectional `ns16550a` at GPA `0x700FF000` with byte-spaced registers and SPI 44, selects it in `/chosen/stdout-path`, and disables guest USB, PADCTL, its mailbox, USB-C role control, and USB power domains.

For the Scarlet Switch Console entry, put the overlay at `/switchroot/scarlet-console/usb-uart.dtbo`. Copy the entry's `boot.cmd`, and insert the following immediately before `bootm`, after the OS DTB has been selected and configured:

```sh
if itest.l *70019d4c == fec00000; then
    if load mmc ${devnum}:${distro_bootpart} 0x8c000000 ${boot_dir}/usb-uart.dtbo && fdt addr ${fdtraddr} && fdt resize 8192 && fdt apply 0x8c000000; then
        echoe Switchvisor USB console enabled
    else
        echoe Failed to apply Switchvisor USB console overlay
        sleep 3
        reset
    fi
fi
```

The GSC5 check selects the Switchvisor path; a native boot entry using this script keeps its original console and USB settings. Package the edited script into a new file:

```sh
cargo run -p switchvisor-tool -- pack-script path/to/boot.cmd dist/boot.scr
```

Copy that `boot.scr` to `/switchroot/scarlet-console/boot.scr` alongside the overlay and BL33. The packer refuses to overwrite an existing output file; use a new output path when rebuilding it.

The pinned native BL33 already supports `fdt apply`. For another guest DTB, declare the same UART and disable that DTB's physical USB/PHY/role nodes; the supplied overlay targets the Noble node paths.

Connect the Switch to a host with a USB data cable. On macOS, open the new CDC ACM port:

```sh
ls /dev/cu.usbmodem*
USB_PORT=/dev/cu.usbmodemSWV00011
screen "$USB_PORT" 115200
```

Set `USB_PORT` to the actual port name printed by `ls`.

USB transmits the guest's UART bytes unchanged. Handle terminal newline conversion in the guest console or TTY layer, or configure it in the host terminal.

With `--usb-uart`, Switchvisor polls USB for two seconds before starting the payload and prints the port/endpoint state, event/setup counts, and last error on Hekate's framebuffer. Connect the host cable before boot to capture enumeration progress. Boot continues when this probe finishes even if no host is present.

Baud and line-coding settings are USB metadata. Guest transmit uses THR and polls LSR.THRE/TEMT; both stay ready even when the host is absent. Host input enters the receive FIFO, updates LSR.DR, and raises the UART receive interrupt when enabled through IER. The UART buffers up to 64 KiB of guest output and 4 KiB of host input. Later output bytes are dropped if the TX queue fills; completed USB input is backpressured until the guest makes RX space. Opening the host port asserts DTR and drains queued output.

XUDC events use physical SPI 44 (architectural INTID 76), which EL2 services before delivering the same hardware-backed interrupt as the virtual UART RX line. Guest WFI remains native and USB traffic wakes EL2 through the physical interrupt. The boot probe and trapped guest exits retain bounded polling as a fallback. Guest USB controller accesses return an absent bus, while shared clock/reset/PMC writes preserve USB-owned resources and the XUDC SMMU client stays in bypass.

## Payload entry contract

Supply a raw binary that can execute at the fixed BL33 load address. Its file bytes are copied unchanged, and the remaining memory through `runtime-size` is cleared.

| Item | Value |
|---|---|
| Load address | `0xAA000000` |
| Entry address | Load address plus `--entry-offset`; 4-byte aligned and inside the file |
| CPU state | AArch64 EL1h, DAIF masked, MMU and caches off |
| Guest address space | GPA=HPA, 36-bit Stage-2; VMM RAM `0xFEC00000`–`0xFFC00000` is unmapped |
| Memory discovery | MC page `0x70019000` is trapped; disabled GSC5 advertises the VMM reservation |
| Interrupts | Physical IRQs forwarded through the GICv2 virtual CPU interface; FIQ/SError remain at EL1 |
| Firmware calls | PSCI CPU_ON, CPU_OFF, CPU-level AFFINITY_INFO and their FEATURES queries; other native SMCs forwarded; suspend unsupported |
| x0-x7 | Original BL31 inputs, or explicit register options |
| x8-x30 | Zero |
| Initial stack | SP_EL1=`0x8A800000`; the preceding 64 KiB are cleared |
| Size limits | Composite package and payload runtime each at most 64 MiB |

Package overhead reduces the maximum raw file size. The Hekate environment window preceding `0xAA000000` is preserved. Payloads must keep any required pointed-to data outside the composite package that is overwritten during installation.

MC emulation supports aligned 32-bit loads/stores with valid AArch64 abort syndrome information, including signed loads. The virtual GSC5 address and size registers are read-only; other accesses in the MC page pass through to hardware. Unsupported MC accesses stop the payload.

The initial BL33 runs on CPU0. A guest may start CPUs 1–3 through PSCI using MPIDR affinities 1–3. Each CPU enters EL2, installs private stacks/vectors and the shared memory maps, then enters the supplied guest address at EL1h with MMU/caches off and DAIF masked. x0 receives the context, x1-x30 are zero, and SP_EL1 is zero; the secondary entry must install its own stack. CPU_ON entry addresses must be 4-byte aligned, in Normal guest memory, and outside the VMM region. CPU_OFF leaves the physical CPU waiting in EL2 for another virtual CPU_ON. Physical timers and IPIs remain assigned to the guest and are forwarded through vGICv2.

Guest software keeps the platform GICv2 MMIO addresses. Stage-2 maps the guest GICC range to the hardware GICV interface, so guest interrupt acknowledge and EOI do not exit to EL2. A physical IRQ enters EL2 once to populate a hardware List Register; the GIC then handles guest delivery and PPI/SPI deactivation.

Keep the VMM region out of bootloader allocations, OS memory banks, and device DMA buffers. A CPU access to this region faults at EL2 and parks the faulting CPU. Stage-2 does not constrain device DMA. The fixed VMM placement requires usable RAM throughout that region on the target boot configuration.

## Development

```sh
cargo test --workspace
cargo check-el2
cargo build-hv
```

## License

Switchvisor is licensed under the GNU General Public License version 2 only. See [LICENSE](LICENSE).
See [THIRD_PARTY.md](THIRD_PARTY.md) for Hekate attribution.
