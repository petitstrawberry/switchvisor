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

## Implementation bounds

- NCM 1.0, USB 2 full/high speed, NTB16 without CRC. IN sends one frame per NTB;
  OUT accepts up to 16 datagrams in a 16 KiB block, including chained NDPs.
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
It does not emulate Tegra XUDC. Host tests drive the actual XUDC event-ring code
with simulated MMIO and DMA, covering NCM enumeration, control requests, both
transfer directions through virtio-net, short packets/ZLP, malformed input,
reset, disconnect, and coexistence with existing USB channels.
