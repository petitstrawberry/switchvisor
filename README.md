# switchvisor

A lightweight hypervisor for Nintendo Switch.

Switchvisor occupies the platform's BL33 slot, establishes EL2 isolation, and
starts one guest entry at EL1. That entry normally fills the guest's BL33 role,
for example with U-Boot, but its raw image, address, and supporting memory layout
are selected by the caller.

The current platform integration boots through Hekate L4T and a compatible
BL31. Early boot messages use Hekate's framebuffer.

```text
Hekate -> BL31 -> Switchvisor (EL2) -> guest entry (EL1)
```

## Build environment

`flake.nix` provides the Rust toolchain and development tools.
From the repository root, enter the Nix development shell:

```sh
nix develop
```

Run the following build and development commands inside this shell.

## Building a payload

Prepare a raw AArch64 binary and a Hekate L4T bootstack directory containing
`bl31.bin`, `bl33.bin`, and `nx-plat.dtimg`. The directory may be anywhere, but
its files must match the
[pinned bootstack configuration](crates/switchvisor-tool/config/bootstack.json);
the packager uses them to verify the bootstack and extract the Hekate probe FDT.
Supply the EL1 payload as a separate raw file.

```text
scripts/build-payload.sh <payload.raw> <runtime-size> <bootstack-directory> [output-directory] [payload options...]
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

The bootstack directory is required. An optional output-directory argument
overrides `dist/`. The three output files are replaced after the build and
packaging succeed. A build with `--usb-uart` also writes `usb-uart.dtbo`. Pass
`--usb-control` to enable USB control and guest bundle loading without adding a
virtual UART to the guest; that profile writes `usb-control.dtbo`. The build
removes stale overlay variants from the output directory. Add `--no-fallback`
with a USB option to require an uploaded payload instead of starting the
packaged payload automatically.

Copy `dist/bl33.bin` to the microSD path configured for BL33 by the selected
Hekate L4T boot entry.

### Using U-Boot

The BL33 from the supported bootstack can be used directly as the external
payload:

```sh
scripts/build-payload.sh path/to/bootstack/bl33.bin 0x68200 path/to/bootstack
```

The runtime size above applies to the pinned BL33 in the bootstack configuration. For another image, supply its own full runtime extent.

U-Boot can retain its normal microSD layout and boot script. It discovers RAM
through the virtualized MC registers, keeps its relocation and allocations below
`0xFEC00000`, and writes the reduced memory banks into the OS DTB. No U-Boot
source patch, control-DTB preparation, or additional boot-script reservation is
required for this memory policy.

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

Enable the EL2-owned USB composite device and its guest CDC ACM transport when
packaging:

```sh
scripts/build-payload.sh path/to/bootstack/bl33.bin 0x68200 path/to/bootstack dist --usb-uart
```

Copy `dist/bl33.bin` to the configured BL33 path. This build also produces
`dist/usb-uart.dtbo`, an overlay for the supported ODIN/Erista platform DTB. It
adds a bidirectional `ns16550a` at GPA `0x700FF000` with byte-spaced registers and
SPI 44, selects it in `/chosen/stdout-path`, and disables guest USB, PADCTL, its
mailbox, USB-C role control, and USB power domains.

Place the overlay on the microSD card. Apply it to the final working DTB after
that DTB has been selected and configured, immediately before `bootm`. A U-Boot
command sequence has this form:

```sh
load <interface> <device[:partition]> 0x8c000000 <path>/usb-uart.dtbo
fdt addr <working-fdt-address>
fdt resize 8192
fdt apply 0x8c000000
```

Replace the placeholders with the storage and DTB values used by the boot
script. Use a dedicated Switchvisor boot entry so a native entry retains its
original console and USB settings. Package the edited source command file into a
new `boot.scr`:

```sh
cargo run -p switchvisor-tool -- pack-script path/to/boot.cmd dist/boot.scr
```

Install the resulting `boot.scr` at the path loaded by the Hekate entry. The
packer refuses to overwrite an existing output file; use a new output path when
rebuilding it.

The supported U-Boot payload provides `fdt apply`. The supplied overlay targets
the supported ODIN/Erista node paths. A different guest DTB must declare the same
UART and disable its physical USB, PHY, and role-control nodes.

Connect the Switch to a host with a USB data cable. The composite device exposes
separate guest-console and Switchvisor-control CDC ports. On macOS, open the
guest-console port with minicom:

```sh
ls /dev/cu.usbmodem*
USB_PORT=/dev/cu.usbmodemSWV00011
minicom -D "$USB_PORT" -b 115200
```

Set `USB_PORT` to the actual port name printed by `ls`.

USB transmits the guest's UART bytes unchanged. Handle terminal newline conversion in the guest console or TTY layer, or configure it in the host terminal.

With `--usb-uart`, `--usb-control`, or `--usb-gdb`, Switchvisor normally services USB for two
seconds before starting the packaged payload and prints the port/endpoint
state, event/setup counts, and last error on Hekate's framebuffer. Connect the
host cable before boot to capture enumeration progress.

Baud and line-coding settings are USB metadata. Guest transmit uses THR and polls LSR.THRE/TEMT; both stay ready even when the host is absent. Host input enters the receive FIFO, updates LSR.DR, and raises the UART receive interrupt when enabled through IER. The UART buffers up to 64 KiB of guest output and 4 KiB of host input. Later output bytes are dropped if the TX queue fills; completed USB input is backpressured until the guest makes RX space. Opening the host port asserts DTR and drains queued output.

XUDC events use physical SPI 44 (architectural INTID 76), which EL2 services before delivering the same hardware-backed interrupt as the virtual UART RX line. Guest WFI remains native and USB traffic wakes EL2 through the physical interrupt. The boot probe and trapped guest exits retain bounded polling as a fallback. Guest USB controller accesses return an absent bus, while shared clock/reset/PMC writes preserve USB-owned resources and the XUDC SMMU client stays in bypass.

## USB control and guest bundle loading

Enable the management interfaces while packaging. Add `--usb-uart` as well if
the guest needs the virtual console and its DT overlay.

```sh
scripts/build-payload.sh path/to/default.raw 0x100000 path/to/bootstack dist \
  --usb-control
```

For a required-upload boot that never starts the packaged payload automatically,
add `--no-fallback`:

```sh
scripts/build-payload.sh path/to/default.raw 0x100000 path/to/bootstack dist \
  --usb-control --no-fallback
```

This mode waits for an upload and `BOOT` without a deadline. If USB cannot be
initialized, Switchvisor reports the failure on the framebuffer and remains in
EL2. The control port stays available for `status`, `reboot`, and `reboot-rcm`
while waiting. `status` reports `fallback=disabled`.

This profile produces `dist/usb-control.dtbo` for the supported ODIN/Erista
platform DTB. Apply it to the final working DTB immediately before boot, using
the same `fdt addr`, `fdt resize`, and `fdt apply` sequence shown for the console
overlay. It disables the guest's physical USB, PHY, role-control, mailbox, and
power-domain nodes without adding a virtual UART.

Build the host utility inside the Nix development shell:

```sh
cargo build -p switchvisorctl --release
```

On macOS, the development shell also provides the pinned `nxboot` command used
for RCM payload injection.

The control port remains available after the guest starts. `switchvisorctl`
selects the CDC function by its USB interface number. If automatic selection is
unavailable, pass `--port /dev/cu.usbmodem...` before the command or set
`SWITCHVISOR_CONTROL_PORT`.

```sh
target/release/switchvisorctl ping
target/release/switchvisorctl status
target/release/switchvisorctl reboot
target/release/switchvisorctl reboot-rcm
```

To initialize several guest RAM regions and boot one of them, place a
`bundle.json` beside the opaque input files:

```json
{
  "version": 1,
  "entry": "0xaa000000",
  "preserve_boot_args": true,
  "images": [
    {
      "path": "bootloader.bin",
      "address": "0xaa000000",
      "runtime_size": "0x100000"
    },
    {
      "path": "kernel.img",
      "address": "0xa0000000"
    },
    {
      "path": "initramfs",
      "address": "0x92000000"
    }
  ]
}
```

Then deploy the directory while Switchvisor is in preboot:

```sh
target/release/switchvisorctl deploy path/to/bundle
```

`deploy` waits up to 15 seconds for the USB device, uploads every image,
verifies each CRC32, commits the complete bundle, and boots its EL1 entry.
`runtime_size` defaults to the file size; a larger value zero-fills the tail.
`switchvisorctl hello` reports the accepted low guest RAM envelope. Images must
fit within that envelope, must not overlap each other, and cannot overwrite the
active initial EL1 stack or inherited framebuffer. Switchvisor assigns no
meaning to image names or contents. A bootloader-specific handoff block or magic
value can therefore be generated by an external project and included as another
image.

The bundle must designate an aligned entry inside the file extent of one
uploaded image. This image is the guest's initial firmware entry and normally
serves the BL33 role, but Switchvisor does not require U-Boot or a BL33-specific
file format. By default the original BL31 x0-x7 values are preserved. Set
`preserve_boot_args` to `false` for zero registers, or provide exactly eight
numeric values in `registers` for explicit x0-x7. JSON numbers and strings may
use decimal notation; strings may also use `0x` hexadecimal notation.

For a single replacement at the conventional BL33 address, the compatibility
command builds a one-image bundle and leaves it ready for an explicit boot:

```sh
target/release/switchvisorctl upload-bl33 path/to/payload.raw \
  --runtime-size 0x100000 --entry-offset 0
target/release/switchvisorctl boot
```

`runtime-size`, entry, and x0-x7 follow the same raw EL1 contract as packaged
payloads. With no `--x0` through `--x7` options, the original BL31 registers are
preserved. Once a transfer begins, the packaged fallback is no longer safe to
use because guest RAM may have been overwritten. `abort` discards the transfer
and permits a new one; it does not restore overwritten memory. Without
`--no-fallback`, the packaged payload starts normally if no transfer begins in
the preboot window. Transfers are rejected after guest execution begins.

The loader status and abort operations are also available directly:

```sh
target/release/switchvisorctl hello
target/release/switchvisorctl loader-status
target/release/switchvisorctl abort
```

## EL2 GDB debugging

Add `--usb-gdb` when building the payload to expose a dedicated GDB CDC port.
It can be combined with `--usb-uart` and `--usb-control`. Without `--usb-uart`,
the build writes `dist/usb-control.dtbo`; apply that overlay to the final guest
DTB so the guest does not drive the physical USB controller.

```sh
scripts/build-payload.sh path/to/bootstack/bl33.bin 0x68200 path/to/bootstack dist \
  --usb-gdb
target/release/switchvisorctl gdb-port
```

Open the reported port from an AArch64-capable GDB after the guest starts:

```gdb
file path/to/guest.elf
target remote /dev/cu.usbmodem...
set scheduler-locking on
info threads
```

On macOS, LLDB can reach the serial port through a local TCP bridge:

```sh
socat TCP-LISTEN:2159,bind=127.0.0.1,reuseaddr \
  FILE:/dev/cu.usbmodem...,rawer,echo=0,ispeed=115200,ospeed=115200
```

In another terminal, run `lldb path/to/guest.elf` and enter `gdb-remote 2159`.

GDB thread IDs 1–4 correspond to physical CPUs 0–3. Opening the serial port
alone does not stop the guest; the first GDB packet starts an all-stop session.
Registers, `continue`, `stepi`, software breakpoints, and guest RAM reads and
writes are supported. `detach` resumes the guest and removes breakpoints.
`switchvisorctl status` reports the debugger state and last stop.

GDB memory and software-breakpoint addresses inside guest RAM are treated as
physical addresses (`GPA = HPA`) for compatibility. Other addresses, including
high kernel virtual addresses, are translated through CPU0's current EL1 and
stage-2 page tables. Low guest RAM below the Switchvisor reservation and the
high DRAM bank reported by the memory controller are accessible; MMIO and
Switchvisor's resident memory remain excluded. Kernel mappings shared across
CPUs work with this model, while per-process address spaces on other CPUs are
not yet supported. SGI 15 is reserved for EL2 while `--usb-gdb` is enabled.

### Automatic payload cycle

With a no-fallback Switchvisor entry installed on the microSD card, a single
command can return a running guest to RCM, inject Hekate, select the Switchvisor
entry by ID, upload a raw EL1 payload or guest bundle, and boot it:

```sh
scripts/run-payload.sh path/to/hekate.bin path/to/payload.raw 0x100000 \
  --entry-offset 0

scripts/run-payload.sh path/to/hekate.bin --bundle path/to/bundle
```

The Hekate entry ID defaults to `SWV-NX`. Set `SWITCHVISOR_HEKATE_ID` when the
installed entry uses another ID. `SWITCHVISOR_CONTROL_PORT` and the upload
register options work as described above. The Hekate payload path is always
explicit; the script does not depend on a local Hekate checkout or download
location. On macOS the script waits for APX enumeration and IOKit interface
readiness before invoking nxboot.

## Guest entry contract

The packaged fallback and `upload-bl33` command use a raw binary at the fixed
BL33 load address. A deployed bundle may choose another EL1 entry within any of
its uploaded images. File bytes are copied unchanged, and each runtime tail is
cleared through its declared `runtime_size`.

| Item | Value |
|---|---|
| Image addresses | Packaged/single upload: `0xAA000000`; bundle: selected per image by its manifest |
| Entry address | Packaged/single upload: load address plus `--entry-offset`; bundle: an aligned address inside one uploaded file |
| CPU state | AArch64 EL1h, DAIF masked, MMU and caches off |
| Guest address space | GPA=HPA, 36-bit Stage-2; VMM RAM `0xFEC00000`–`0xFFC00000` is unmapped |
| Memory discovery | MC page `0x70019000` is trapped; disabled GSC5 advertises the VMM reservation |
| Interrupts | Physical IRQs forwarded through the GICv2 virtual CPU interface; FIQ/SError remain at EL1 |
| Firmware calls | PSCI CPU_ON, CPU_OFF, CPU-level AFFINITY_INFO and their FEATURES queries; other native SMCs forwarded; suspend unsupported |
| x0-x7 | Original BL31 inputs, or explicit register options |
| x8-x30 | Zero |
| Initial stack | SP_EL1=`0x8A800000`; the preceding 64 KiB are cleared |
| Size limits | Composite package and conventional BL33 runtime each at most 64 MiB; bundle images must fit accepted guest RAM |

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
