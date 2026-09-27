//! Shared virtio-net state; lock order is USB -> NETWORK, never the reverse.
use crate::{arch::aarch64::sync::Mutex, platform::tegra210::io::Hardware};
use core::{
    arch::asm,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use switchvisor::{
    drivers::{Clock, Mmio},
    mmio::Access,
    net::{Ethernet, Network},
    vdev::{
        self,
        virtio_net::{BASE, GuestMemory, MemoryError, SIZE},
    },
};

struct State {
    network: Network,
    calls: u64,
    total_us: u64,
    max_us: u64,
    lock_max_us: u64,
}
static NETWORK: Mutex<State> = Mutex::new(State {
    network: Network::new(),
    calls: 0,
    total_us: 0,
    max_us: 0,
    lock_max_us: 0,
});
static ENABLED: AtomicBool = AtomicBool::new(false);
static PENDING: AtomicBool = AtomicBool::new(false);
static HIGH_END: AtomicU64 = AtomicU64::new(0x1_0000_0000);

pub fn configure(enabled: bool) {
    if enabled {
        HIGH_END.store(
            switchvisor::mc::guest_high_end(|offset| {
                Hardware.read32(switchvisor::mc::BASE + offset)
            }),
            Ordering::Release,
        );
    }
    ENABLED.store(enabled, Ordering::Release);
}
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}
pub fn interrupt_pending() -> bool {
    PENDING.load(Ordering::Acquire)
}

/// Called in USB -> NETWORK lock order, like packet service. No counters reset.
pub fn diagnostics() -> [u64; 9] {
    unsafe {
        NETWORK.with(|state| {
            let network = &state.network;
            [
                network.bridge.host_received,
                network.device.rx_packets,
                network.bridge.guest_rx_dropped,
                network.bridge.guest_high_water as u64,
                network.bridge.rejected,
                state.calls,
                state.total_us,
                state.max_us,
                state.lock_max_us,
            ]
        })
    }
}

struct Memory;
impl GuestMemory for Memory {
    fn valid(&self, address: u64, length: usize) -> bool {
        switchvisor::loader::guest_range(address, length as u64)
            || (address >= 0x1_0000_0000
                && length != 0
                && address
                    .checked_add(length as u64)
                    .is_some_and(|end| end <= HIGH_END.load(Ordering::Acquire)))
    }
    fn read(&mut self, address: u64, output: &mut [u8]) -> Result<(), MemoryError> {
        if !self.valid(address, output.len()) {
            return Err(MemoryError);
        }
        unsafe {
            // The guest maps RAM cacheable while EL2 uses a noncacheable alias.
            // Clean to PoC before EL2 reads, without discarding guest writes.
            cache_range(address, output.len(), false);
            crate::arch::aarch64::copy::read(address as *const u8, output);
        }
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryError> {
        if !self.valid(address, bytes.len()) {
            return Err(MemoryError);
        }
        unsafe {
            cache_range(address, bytes.len(), true);
            crate::arch::aarch64::copy::write(address as *mut u8, bytes);
            asm!("dsb sy", options(nostack));
            cache_range(address, bytes.len(), true);
        }
        Ok(())
    }
    fn barrier(&mut self) {
        Hardware.barrier();
    }
}

unsafe fn cache_range(address: u64, length: usize, invalidate: bool) {
    // Cortex-A57 D-cache line is 64 bytes. Both operations broadcast for the
    // inner-shareable guest mapping; barriers complete them before copying.
    let end = address + length as u64;
    for line in (address & !63..end).step_by(64) {
        unsafe {
            if invalidate {
                asm!("dc civac, {line}", line = in(reg) line, options(nostack));
            } else {
                asm!("dc cvac, {line}", line = in(reg) line, options(nostack));
            }
        }
    }
    unsafe {
        asm!("dsb sy", options(nostack));
    }
}

pub fn service(ethernet: &mut impl Ethernet) {
    if !enabled() {
        return;
    }
    let requested = Hardware.now_us();
    unsafe {
        NETWORK.with(|state| {
            let started = Hardware.now_us();
            let network = &mut state.network;
            network.service(&mut Memory, ethernet, started);
            PENDING.store(network.device.interrupt_pending(), Ordering::Release);
            let elapsed = Hardware.now_us().wrapping_sub(started);
            state.calls = state.calls.saturating_add(1);
            state.total_us = state.total_us.saturating_add(elapsed);
            state.max_us = state.max_us.max(elapsed);
            state.lock_max_us = state.lock_max_us.max(started.wrapping_sub(requested));
        })
    }
}

pub fn emulate(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    if !enabled() {
        return false;
    }
    // Unrelated MMIO (including the guest GIC) must not contend with packet I/O.
    let Some(access) = Access::decode_region(esr, far, hpfar, BASE, SIZE) else {
        return false;
    };
    let service = access.write && matches!(access.offset, 0x44 | 0x50 | 0x70);
    let handled = unsafe {
        NETWORK.with(|state| {
            let network = &mut state.network;
            let handled = vdev::emulate_access(&mut network.device, access, registers);
            PENDING.store(network.device.interrupt_pending(), Ordering::Release);
            handled
        })
    };
    // Queue/status changes may make progress. Reads and ACK only inspect or
    // update registers; they must not synchronously drain packets as a side effect.
    if handled && service {
        crate::usb::service_network();
    }
    handled
}

pub struct Disconnected;
impl Ethernet for Disconnected {
    fn link_up(&self) -> bool {
        false
    }
    fn receive_frame(&mut self, _: impl FnOnce(&[u8]) -> bool) -> bool {
        false
    }
    fn send_frame(&mut self, _: &[u8]) -> bool {
        false
    }
}
