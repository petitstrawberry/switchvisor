//! Shared guest distributor state. Ordinary IRQ forwarding never takes this lock.

use crate::{arch::aarch64::sync::Mutex, platform::tegra210::io::Hardware};
use core::sync::atomic::{AtomicBool, Ordering};
use switchvisor::{
    drivers::interrupt::gicv2::Layout,
    vdev::{self, gicv2::Distributor},
};

static DISTRIBUTOR: Mutex<Option<Distributor<Hardware>>> = Mutex::new(None);
static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn initialize(layout: Layout) {
    let mut distributor = Distributor::new(Hardware, layout);
    distributor.initialize();
    ENABLED.store(distributor.enabled(), Ordering::Release);
    unsafe {
        DISTRIBUTOR.with(|state| *state = Some(distributor));
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn emulate(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    unsafe {
        DISTRIBUTOR.with(|state| {
            let Some(distributor) = state else {
                return false;
            };
            let handled = vdev::emulate(distributor, esr, far, hpfar, registers);
            if handled {
                ENABLED.store(distributor.enabled(), Ordering::Release);
            }
            handled
        })
    }
}

/// Handle the narrow instruction-decoding fallback used when Cortex-A57 does
/// not report an instruction syndrome for a post-index distributor store.
pub fn emulate_store_post_index(
    esr: u64,
    far: u64,
    hpfar: u64,
    instruction: u32,
    registers: &mut [u64; 31],
) -> bool {
    unsafe {
        DISTRIBUTOR.with(|state| {
            let Some(distributor) = state else {
                return false;
            };
            let region = vdev::VirtualDevice::region(distributor);
            let Some((access, writeback)) =
                switchvisor::mmio::Access::decode_store_post_index_region(
                    esr,
                    far,
                    hpfar,
                    instruction,
                    region.base,
                    region.size,
                    registers,
                )
            else {
                return false;
            };
            if !vdev::emulate_access(distributor, access, registers) {
                return false;
            }
            registers[writeback.register] = writeback.value;
            ENABLED.store(distributor.enabled(), Ordering::Release);
            true
        })
    }
}
