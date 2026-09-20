//! Shared virtio-net state; lock order is USB -> NETWORK, never the reverse.
use crate::{arch::aarch64::sync::Mutex, platform::tegra210::io::Hardware};
use core::{
    arch::asm,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use switchvisor::{
    drivers::Mmio,
    mmio::Access,
    net::{Ethernet, FRAME_SIZE, Network},
    vdev::{
        self,
        virtio_net::{BASE, GuestMemory, MemoryError, SIZE},
    },
};

static NETWORK: Mutex<Network> = Mutex::new(Network::new());
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
            for (i, byte) in output.iter_mut().enumerate() {
                *byte = core::ptr::read_volatile((address + i as u64) as *const u8);
            }
        }
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryError> {
        if !self.valid(address, bytes.len()) {
            return Err(MemoryError);
        }
        unsafe {
            cache_range(address, bytes.len(), true);
            for (i, byte) in bytes.iter().enumerate() {
                core::ptr::write_volatile((address + i as u64) as *mut u8, *byte);
            }
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
    unsafe {
        NETWORK.with(|network| {
            network.service(&mut Memory, ethernet);
            PENDING.store(network.device.interrupt_pending(), Ordering::Release);
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
        NETWORK.with(|network| {
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
    fn receive_frame(&mut self, _: &mut [u8; FRAME_SIZE]) -> usize {
        0
    }
    fn send_frame(&mut self, _: &[u8]) -> bool {
        false
    }
}
