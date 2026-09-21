# USB networking bring-up

`--usb-net` adds a CDC-NCM function to the existing EL2-owned USB composite
and a virtio-net MMIO device to the guest. The control CDC and bundle loader
remain available. `--usb-uart` can also be enabled; its interrupt is separate.

```text
Mac / PC (192.168.77.2/24)
  USB CDC-NCM, host MAC 02:53:56:00:00:01
      |
switchvisor Ethernet bridge
  management 192.168.77.1, MAC 02:53:56:00:00:03
      |
virtio-net (guest MAC 02:53:56:00:00:02)
  Scarlet / another guest (suggested 192.168.77.3/24)
```

This is an Ethernet bridge with a local management endpoint. Guest traffic is
forwarded unchanged. DHCP, NAT, host Internet sharing and IP routing are not
configured by Switchvisor. Addresses above are a static bring-up convention;
only the VMM address is fixed by this implementation. Use a host interface on
which `192.168.77.0/24` does not conflict with another network.

## Build and guest discovery

```sh
scripts/build-payload.sh path/to/bootstack/bl33.bin 0x68200 path/to/bootstack dist --usb-net
```

The example runtime size applies to the pinned U-Boot. For an opaque guest
payload, provide its own full runtime size. Add `--usb-uart` for a console or
`--no-fallback` to require a bundle upload. The manifest records both USB and
virtio parameters.

Network payloads require the current `SVBOOT05` bootstrap. The packager rejects
`--usb-net` with an older bootstrap; rebuild it with the script above.

Apply `usb-net.dtbo` to the final guest DTB immediately before boot, as for the
other overlays documented in the README. It disables guest physical USB,
PADCTL, mailbox, role control and USB power domains, and adds:

| Property | Value |
| --- | --- |
| Compatible | `virtio,mmio` |
| GPA / size | `0x700fe000` / `0x1000` |
| Transport version / device ID | 2 / 1 (network) |
| Interrupt parent / specifier | ODIN `lic` / `<0 39 4>` (INTID 71) |
| Features | `VIRTIO_F_VERSION_1`, MAC, link status |
| Queues | RX 0, TX 1; split rings, at most 128 entries each |
| Packet header | 12 bytes, including `num_buffers` |
| MTU | 1500; Ethernet frames at most 1514 bytes without FCS |

The virtual network line reuses the disabled XUSB host SPI 39. XUDC/console
continues to use SPI 44 (INTID 76). EL2 protects both interrupt sources in GIC
and LIC and services physical XUDC events before guest delivery. Delivery is
pinned to CPU0, following the existing USB console model.

When combining `--usb-net --usb-uart`, the builder emits **both**
`usb-net.dtbo` and `usb-uart.dtbo`; apply both to the guest DTB. Applying the
network overlay alone does not change `/chosen/stdout-path`.

## Interrupts and guest latency

- Physical XUDC INTID 76 runs on CPU0 in EL2. USB completions and management
  unicast traffic are handled there; the guest receives INTID 71 for virtio
  queue completion or configuration changes. The optional UART has its own
  virtual INTID 76. Broadcast frames, including management ARP discovery, are
  still forwarded across the Ethernet bridge and can cause guest RX interrupts.
- Guest notification obeys virtqueue `NO_INTERRUPT`, virtual GIC/LIC enables,
  and the guest priority mask. Owned virtual interrupts have an initial priority
  of `0xa0`; guest writes to their GIC priority registers affect virtual delivery
  without changing EL2's physical USB service priority. Multiple completions
  share the pending virtio interrupt bits; there is no timed interrupt coalescing
  or EVENT_IDX support yet.
- A notification-only INTID 71 does not poll XUDC or acquire the USB/network
  locks. Unrelated MMIO is rejected before acquiring the network lock. Virtio
  register reads and ACKs do not directly drain queues; explicit queue/status
  writes can service them. Processing stops as soon as a round makes no progress,
  with a maximum of 16 rounds per call.
- Once a virtqueue is observed empty, it waits for its `QueueNotify` instead
  of reading guest RAM again on every USB service. Queue enable and entry into
  `DRIVER_OK` also schedule an initial check. Queues with remaining descriptors
  stay runnable across the service budget and host egress backpressure. Available
  buffer notifications are never suppressed; guests must notify RX after
  replenishing buffers and TX after posting packets, as required by Virtio 1.2
  section 2.7.10.1. Console and descriptor scratch storage is retained between
  calls. Ethernet payloads use borrowed DMA storage or final ring slots.
- CPU0 resumes its guest after EL2 service. Other guest CPUs are not deliberately
  stopped, but simultaneous access to shared USB/network state can wait on its
  lock. Packet copies and cache maintenance still consume time with local
  interrupts masked. The round limit is **not a wall-clock latency guarantee**.
- IRQ routing uses a physical SPI to back each virtual level. It therefore
  includes an EL2 notification entry and an EOI recheck entry. There is no new
  periodic timer: the existing USB fallback poll is attempted on MMIO/SMC exits,
  at most once per 250 microseconds, in addition to physical IRQ service.

The existing hypervisor already routes physical IRQs through EL2; `--usb-net`
does not introduce that global routing policy. Real-device service duration,
timer latency, interrupt rate and SMP lock contention remain to be measured.

## Scarlet compatibility

The initial Scarlet revision inspected was
`1eddc988a5c741f9bbb9a4d05a1cc25f7419badf`. Its driver requires MMIO version 2,
but omits `VIRTIO_F_VERSION_1` from accepted network features and always uses a
10-byte network header. Modern virtio requires VERSION_1 and a 12-byte header,
even without mergeable receive buffers. See the
[Virtio 1.2 network specification](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html).

[The companion patch](patches/scarlet-virtio-net-v1.patch) makes those two changes
in Scarlet's network driver, including TX buffer layout and RX sizing. It keeps
the 10-byte header for a transport that does not negotiate VERSION_1. It is also
submitted as [Scarlet PR #569](https://github.com/petitstrawberry/Scarlet/pull/569),
including a modern-header regression assertion. In an isolated worktree based
on current `dev`, the AArch64 library check and all 1288 RISC-V QEMU kernel tests
passed using Scarlet's pinned Nix environment. The original Scarlet working
checkout was preserved. The PR is now merged as
`2907183585d869158f70f2116c89950d2907fdae`; current Scarlet `dev` includes it.

Only for an older Scarlet checkout that does not contain that change:

```sh
git apply --check /path/to/switchvisor/docs/patches/scarlet-virtio-net-v1.patch
git apply /path/to/switchvisor/docs/patches/scarlet-virtio-net-v1.patch
```

Rebuild Scarlet and supply a DTB with the virtio node. Do not use a modern
transport with the unpatched, 10-byte-only driver: feature negotiation is
intentionally rejected instead of interpreting two Ethernet bytes as a header.

## Host and management checks (when the Switch is available)

After connecting a data cable and booting the new payload, locate the new USB
Ethernet interface and assign `192.168.77.2/24`. For example, on macOS:

```sh
networksetup -listallhardwareports
sudo ifconfig enX inet 192.168.77.2 netmask 255.255.255.0 up
ping 192.168.77.1
printf 'ping\n' | nc -u -w 1 192.168.77.1 7777
printf 'status\n' | nc -u -w 1 192.168.77.1 7777
```

Replace `enX` with the actual NCM interface. UDP `ping` returns `pong`; `status`
returns endpoint identity, management IP and MTU. This endpoint is read-only;
reboot and uploads continue to use `switchvisorctl` over the existing USB
control/loader functions. A guest is not needed for these management replies.

Set the guest to `192.168.77.3/24`, then check host ↔ guest ping and VMM ping
from both sides. Verify simultaneous console traffic if using `--usb-uart`.
Unplug/replug and repeat after guest network reset. The following bring-up
records macOS enumeration and basic guest traffic; USB reset/replug stress,
interrupt latency, SMP contention and sustained throughput remain unmeasured.

### Switch / macOS bring-up, 2026-09-21

The USB-NCM interface enumerated as `en13`, with the host at `.2` and Scarlet
`veth0` at `.3`. Management ICMP and UDP ping/status worked before guest boot.
Guest and host ICMP worked in both directions, and a guest HTTP server served
its 6619-byte index with a matching SHA-256.

The first 1 MiB host-to-guest TCP transfer exposed a deadlock: guest RX filled
the bridge while Scarlet waited for synchronous TX completion holding its
combined RX/TX lock. Gating TX on RX capacity prevented TCP ACK completion.
The fix gates guest TX only on host egress capacity. The regression
`full_guest_rx_queue_does_not_block_guest_tx` failed before this change and
passes with it; all 112 workspace tests passed.

After deployment, a 1 MiB upload took 2.174 seconds and its download took
1.983 seconds, with exact byte comparison and SHA-256
`f443f5f87314e70000f7cc4715f041d19ba44748d0f705839735ed4cd7c1383c`.
The console remained responsive. These are individual bring-up transfers,
not sustained-throughput measurements. Host PF NAT then enabled guest DNS and
an external HTTP 200 response from `example.com`.

Evidence: `.cache/net-bringup-20260921/tcp-roundtrip-fixed.json`,
`backpressure-before.log` and `tests-after.log`. The tested monitor payload
SHA-256 is `61d28a1cd2f207e2a686f036ab6a7dda5301a73d0522e371954a86e121cb6647`.

### Idle service overhead, 2026-09-22

The empty-queue notification gating and retained scratch buffers were deployed
with USB UART/control/NCM enabled and GDB disabled. Only the SD monitor payload
changed. The guest kernel and initramfs from the earlier measurement were kept
in a separate bundle after detecting a concurrent rebuild of the shared console
package. The archived initramfs was restored with its recorded length, header
CRC and data CRC; the kernel matched its recorded SHA-256.

With no applications opened for the comparison, the earlier connected-host
samples were 39.9%, 39.1%, 37.6% and 37.6% total CPU busy. The first four samples
after the monitor update were 24.0%, 33.5%, 20.7% and 20.5%; after settling,
four more were 21.3%, 19.7%, 20.9% and 20.2% (mean 20.525%). These are short
guest `top` samples across separate boots, not a controlled repeated A/B trial
or a measurement separating the individual optimizations. The user also reported
that interaction felt responsive again.

Management and guest ICMP each completed 5/5 replies. Guest external HTTP
returned 200, and a host-served 1 MiB HTTP body was received in 2.102 seconds.
All 115 workspace tests, Clippy, EL2 ISA validation and four QEMU network cases
passed. The host serial readers were closed after verification.

Monitor payload SHA-256:
`e2ce4c30cec59f6e3ef92309bf6f448d2a41afd51e83673786c45b8914b01faf`.
Evidence is in `.cache/idle-network-fix/` and the companion Switch project's
`.cache/switchvisor-internet/idle-fix-verification.json`, `idle-fix-cpu.json`,
`idle-fix-stable-cpu.json` and `idle-fix-guest-bundle/provenance.json`.

## Implementation bounds

- NCM 1.0, USB 2 full/high speed, NTB16 without CRC. IN batches queued frames
  directly into one DMA NTB, bounded by the host's input size and the eight-slot
  egress queue. A lone frame is sent immediately. OUT accepts up to 16 datagrams
  in a 16 KiB block, including chained NDPs, with two independently owned DMA
  slots so USB reception can overlap CPU consumption.
  Alternate setting 0 stops bulk traffic; setting 1 enables it. EP0 negotiates
  NTB input size and packet filters, and the interrupt endpoint reports link
  and speed changes. Existing CDC and vendor interface numbers do not change.
- Split virtqueues support direct descriptor chains, wraparound, notification
  suppression, reset and interrupt acknowledgement. Indirect descriptors,
  EVENT_IDX, mergeable RX, checksum/GSO offload and jumbo frames are not offered.
- Guest memory access is checked before every copy. Low guest RAM excludes
  the EL2 reservation, inherited framebuffer and initial EL1 stack. High RAM
  uses a boot-time MC snapshot, stopping before the first high firmware carveout.
  Descriptor loops, overflow, wrong direction and invalid queues set
  DEVICE_NEEDS_RESET and raise a configuration interrupt.
- Each bridge egress has eight frame slots, with bounded work per service.
  Guest TX is backpressured only when host egress fills. A full guest RX queue
  must not prevent TX completion: a guest may need to send TCP ACKs before
  recycling receive buffers. Ingress destined for a stalled
  guest may be dropped so host management stays responsive. Link reset flushes
  pending bridge packets. There is no per-packet allocation.

## Monitor memory and packet ownership

EL2 enables its instruction/data caches on every CPU. Only its private resident
RAM is mapped Normal WB, inner-shareable. The separate 2 MiB block starting at
`0xfee00000` stays Normal NC and execute-never; the linker places the 80 KiB USB
DMA arena there. MMIO remains Device-nGnRnE. Guest RAM and the framebuffer retain
their NC EL2 aliases: guests with different stage-1 attributes and MMU-off boot
code do not acquire a new cacheable alias. Guest shared-memory access still
performs the existing cache maintenance, followed by bounded volatile 64-bit
copies with byte handling for alignment/tails. No access extends past the
validated buffer. These memory-type and table-walk controls follow Arm's
[AArch64 memory management guide](https://developer.arm.com/documentation/101811/latest).

CPU0 constructs the immutable maps before enabling caches. Secondary CPUs
install the same mappings before touching shared state. The existing ordered
load/store Bakery synchronization remains; the linked image still prohibits
FP/SIMD and exclusive/atomic RMW instructions, including bootstrap paths.

Normal host-to-guest forwarding borrows a completed XUDC OUT buffer, validates
the entire NTB, and writes its datagram directly to the guest RX descriptor
chain: **one payload copy**, previously five. The DMA slot cannot be rearmed
until the last datagram callback returns. A second queued OUT slot lets USB
receive another NTB meanwhile. If guest RX stalls, the bridge retains a frame
in its bounded queue; that path needs an additional copy and preserves ordering.

Guest TX reads directly into a reserved host egress ring slot, then NCM encodes
directly into its final IN DMA buffer: **two payload copies**, previously five.
The ring snapshot lets the guest recycle a completed TX descriptor while USB
is busy. Both directions process the 12-byte virtio header separately, without
assembling another full-size packet. Descriptor scratch is reused, and only
the validated descriptor count is read. Management replies are generated in
their final ring slots with padding and UDP checksum fields explicitly cleared.

Queued TX frames can share one NTB and one USB completion. Busy IN storage is
not read or rewritten; only frames successfully submitted are removed from the
bridge. Host input-size limits, packet filters, ZLP, endpoint reset, alternate
settings, and ring wrap keep the same ownership/backpressure rules.

### Device verification, 2026-09-22

Payload `94ebc3983ea789b4227e34643c805eb9541dcb51ddb5df1f080658937c999e32`
was installed with SD readback verification. The monitor was the only changed
SD file; 49 protected files were verified unchanged. The uploaded guest booted
all four CPUs with USB UART/control/NCM enabled and no GDB.

Management and guest ICMP each returned 3/3 replies. External HTTP returned
200 in 54 ms. In the same request invocation, a host-served 1 MiB body completed
in **134 ms** (1048576 bytes, HTTP 200). The earlier bring-up record was 2102 ms.
The user requested no repeat of the old-version measurement; these are separate
single-transfer records, with different guest builds and active applications,
not a controlled A/B or sustained-bandwidth claim. `yt` was already running in
the new CPU snapshot, so it is not an idle-CPU comparison.

All 125 workspace tests, Clippy, formatting, the linked ISA guard, four QEMU
network cases, five QEMU SMP cases, and 22 packaging cases passed. QEMU does
not model physical XUDC or prove real cache coherence; the physical checks
above exercise the deployed DMA and guest-memory paths.

Evidence: `.cache/cache-copy-fix/` in this repository and
`.cache/switchvisor-internet/cache-copy-verification.json`,
`cache-copy-http.txt`, `cache-copy-deploy.log`, and `cache-copy-sd-backup/`
in the companion Switch project. The copied guest kernel SHA-256 is
`d57ef5ceb0aa14b66ad0e3e354a7105717f448013d5f5a6f5356389164cb1bbb`;
initramfs SHA-256 is
`e14dbc6b95d3b06ba09b74a3216a8a71a5cdd9cc202c3d8851c3d450fbf798a4`.

## Reproducible verification without a Switch

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build-hv
python3 scripts/check-el2-isa.py target/aarch64-unknown-none-softfloat/release/switchvisor
llvm-objcopy -O binary target/aarch64-unknown-none-softfloat/release/switchvisor /tmp/switchvisor-net.raw
cargo build -p switchvisor-tool
python3 scripts/qemu-net-smoke.py /tmp/switchvisor-net.raw path/to/bootstack --output .cache/qemu-net
python3 scripts/check-payload-packaging.py /tmp/switchvisor-net.raw path/to/bootstack
```

The QEMU output directory must be new. QEMU runs the real EL2 binary with guest
MMU/cache configuration both off and on, checks both virtqueues using a local
management ARP exchange, and verifies virtual INTID 71 delivery and acknowledgement,
priority register readback, injected priority and masking by the guest's GICC_PMR.
It also drains each queue, then verifies that separate TX and RX notifications
resume processing after the queue becomes empty. Host tests check that repeated
idle service performs no guest RAM accesses, and that one notification continues
to make progress across the service budget and egress backpressure.
It does not emulate Tegra XUDC. Host tests drive the actual XUDC event-ring code
with simulated MMIO and DMA, covering NCM enumeration, control requests, both
transfer directions through virtio-net, short packets/ZLP, malformed input,
reset, disconnect, and coexistence with existing USB channels.
