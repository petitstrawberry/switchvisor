//! Shared guest distributor state. Ordinary IRQ forwarding never takes this lock.

use crate::{arch::aarch64::sync::Mutex, platform::tegra210::io::Hardware};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use switchvisor::{
    drivers::interrupt::gicv2::Layout,
    vdev::{
        self,
        gicv2::{DEFAULT_OWNED_PRIORITY, Distributor},
        lic::Lic,
    },
};

static DISTRIBUTOR: Mutex<Option<Distributor<Hardware>>> = Mutex::new(None);
static LIC: Mutex<Option<Lic<Hardware>>> = Mutex::new(None);
static ENABLED: AtomicBool = AtomicBool::new(true);
static NETWORK_INTERRUPT_ENABLED: AtomicBool = AtomicBool::new(false);
static LIC_NETWORK_SOURCE_ENABLED: AtomicBool = AtomicBool::new(true);
static OWNED_INTERRUPT_ENABLED: AtomicBool = AtomicBool::new(false);
static LIC_OWNED_SOURCE_ENABLED: AtomicBool = AtomicBool::new(true);
static NETWORK_INTERRUPT_PRIORITY: AtomicU8 = AtomicU8::new(DEFAULT_OWNED_PRIORITY);
static OWNED_INTERRUPT_PRIORITY: AtomicU8 = AtomicU8::new(DEFAULT_OWNED_PRIORITY);

fn publish_distributor(distributor: &Distributor<Hardware>) {
    NETWORK_INTERRUPT_PRIORITY.store(distributor.network_interrupt_priority(), Ordering::Release);
    OWNED_INTERRUPT_PRIORITY.store(distributor.owned_interrupt_priority(), Ordering::Release);
    NETWORK_INTERRUPT_ENABLED.store(distributor.network_interrupt_enabled(), Ordering::Release);
    OWNED_INTERRUPT_ENABLED.store(distributor.owned_interrupt_enabled(), Ordering::Release);
    ENABLED.store(distributor.enabled(), Ordering::Release);
}

pub fn initialize(layout: Layout, owned_interrupt: Option<u32>, network_interrupt: Option<u32>) {
    let mut distributor = Distributor::new(Hardware, layout);
    distributor.set_owned_interrupt(owned_interrupt);
    distributor.set_network_interrupt(network_interrupt);
    distributor.initialize();
    publish_distributor(&distributor);
    unsafe {
        DISTRIBUTOR.with(|state| *state = Some(distributor));
    }
    let owned_source = (layout == Layout::TEGRA210)
        .then_some(owned_interrupt)
        .flatten()
        .and_then(|interrupt| interrupt.checked_sub(32));
    let network_source = (layout == Layout::TEGRA210)
        .then_some(network_interrupt)
        .flatten()
        .and_then(|id| id.checked_sub(32));
    let mut lic = Lic::new(Hardware);
    lic.set_owned_source(owned_source);
    lic.set_network_source(network_source);
    lic.initialize();
    LIC_OWNED_SOURCE_ENABLED.store(
        owned_source.is_none() || lic.owned_source_enabled(),
        Ordering::Release,
    );
    LIC_NETWORK_SOURCE_ENABLED.store(
        network_source.is_none() || lic.network_source_enabled(),
        Ordering::Release,
    );
    unsafe {
        LIC.with(|state| {
            *state = (owned_source.is_some() || network_source.is_some()).then_some(lic)
        });
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn network_interrupt_enabled() -> bool {
    NETWORK_INTERRUPT_ENABLED.load(Ordering::Acquire)
        && LIC_NETWORK_SOURCE_ENABLED.load(Ordering::Acquire)
}

pub fn network_interrupt_priority() -> u8 {
    NETWORK_INTERRUPT_PRIORITY.load(Ordering::Acquire)
}

pub fn owned_interrupt_priority() -> u8 {
    OWNED_INTERRUPT_PRIORITY.load(Ordering::Acquire)
}

pub fn owned_interrupt_enabled() -> bool {
    OWNED_INTERRUPT_ENABLED.load(Ordering::Acquire)
        && LIC_OWNED_SOURCE_ENABLED.load(Ordering::Acquire)
}

pub fn emulate(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    let distributor = unsafe {
        DISTRIBUTOR.with(|state| {
            let Some(distributor) = state else {
                return false;
            };
            let handled = vdev::emulate(distributor, esr, far, hpfar, registers);
            if handled {
                publish_distributor(distributor);
            }
            handled
        })
    };
    if distributor {
        return true;
    }
    unsafe {
        LIC.with(|state| {
            let Some(lic) = state else {
                return false;
            };
            let handled = vdev::emulate(lic, esr, far, hpfar, registers);
            if handled {
                LIC_OWNED_SOURCE_ENABLED.store(lic.owned_source_enabled(), Ordering::Release);
                LIC_NETWORK_SOURCE_ENABLED.store(lic.network_source_enabled(), Ordering::Release);
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
    let distributor = unsafe {
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
            publish_distributor(distributor);
            true
        })
    };
    if distributor {
        return true;
    }
    unsafe {
        LIC.with(|state| {
            let Some(lic) = state else {
                return false;
            };
            let region = vdev::VirtualDevice::region(lic);
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
            if !vdev::emulate_access(lic, access, registers) {
                return false;
            }
            registers[writeback.register] = writeback.value;
            LIC_OWNED_SOURCE_ENABLED.store(lic.owned_source_enabled(), Ordering::Release);
            LIC_NETWORK_SOURCE_ENABLED.store(lic.network_source_enabled(), Ordering::Release);
            true
        })
    }
}
