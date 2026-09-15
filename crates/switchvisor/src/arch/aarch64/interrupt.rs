//! Per-CPU GICv2 state used directly by the EL2 IRQ vector.

use crate::{platform::tegra210::io::Hardware, vm};
use core::{
    arch::asm,
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use switchvisor::drivers::{
    Driver, InterruptController, Mmio,
    interrupt::gicv2::{Error, Event, GicV2, Layout},
};
use switchvisor::vdev::uart::INTERRUPT_ID;

const HIDREV: u64 = 0x7000_0804;
const TEGRA210_CHIP_ID: u32 = 0x21;

struct PerCpu(UnsafeCell<Option<GicV2<Hardware>>>);
// Each pinned physical CPU exclusively accesses its own entry with DAIF masked.
unsafe impl Sync for PerCpu {}

static GICS: [PerCpu; switchvisor::CPU_COUNT] =
    [const { PerCpu(UnsafeCell::new(None)) }; switchvisor::CPU_COUNT];
static LAYOUT: AtomicU64 = AtomicU64::new(0);
static CONSOLE_INTERRUPT_OWNED: AtomicBool = AtomicBool::new(false);

fn index() -> usize {
    let mpidr: u64;
    unsafe {
        asm!("mrs {mpidr}, mpidr_el1", mpidr = out(reg) mpidr, options(nomem, nostack));
    }
    (mpidr & 0xff) as usize
}

pub fn detect_layout() -> Layout {
    if (Hardware.read32(HIDREV) >> 8) & 0xff == TEGRA210_CHIP_ID {
        Layout::TEGRA210
    } else {
        Layout::QEMU_VIRT
    }
}

pub fn select_layout(layout: Layout) {
    LAYOUT.store(u64::from(layout == Layout::TEGRA210), Ordering::Release);
}

/// Configure ownership before any per-CPU GIC instance is initialized.
pub fn select_console_interrupt(owned: bool) {
    CONSOLE_INTERRUPT_OWNED.store(owned, Ordering::Release);
}

fn selected_layout() -> Layout {
    if LAYOUT.load(Ordering::Acquire) == 1 {
        Layout::TEGRA210
    } else {
        Layout::QEMU_VIRT
    }
}

/// Initialize each pinned CPU once and retain its vGIC state across virtual off/on.
pub fn initialize() -> Result<(), Error> {
    let cpu = index();
    let Some(slot) = GICS.get(cpu) else {
        return Err(Error::NotGicV2);
    };
    let state = unsafe { &mut *slot.0.get() };
    if let Some(gic) = state {
        gic.set_distributor_enabled(vm::interrupt::enabled());
        return Ok(());
    }
    let mut gic = GicV2::new(Hardware, selected_layout());
    gic.set_owned_interrupt(
        CONSOLE_INTERRUPT_OWNED
            .load(Ordering::Acquire)
            .then_some(INTERRUPT_ID),
    );
    gic.initialize()?;
    gic.set_distributor_enabled(vm::interrupt::enabled());
    *state = Some(gic);
    Ok(())
}

/// Assert the physical SPI used to back the virtual UART's level interrupt.
pub fn pend_console_interrupt() {
    if !CONSOLE_INTERRUPT_OWNED.load(Ordering::Acquire) {
        return;
    }
    let cpu = index();
    let Some(slot) = GICS.get(cpu) else {
        return;
    };
    if let Some(gic) = unsafe { &mut *slot.0.get() } {
        gic.pend_owned_interrupt();
    }
}

pub fn synchronize_distributor() {
    let cpu = index();
    let Some(slot) = GICS.get(cpu) else {
        return;
    };
    if let Some(gic) = unsafe { &mut *slot.0.get() } {
        gic.set_distributor_enabled(vm::interrupt::enabled());
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_irq() {
    let cpu = index();
    let Some(slot) = GICS.get(cpu) else {
        return;
    };
    let Some(gic) = (unsafe { &mut *slot.0.get() }) else {
        return;
    };
    gic.set_distributor_enabled(vm::interrupt::enabled());
    if let Event::Owned(interrupt) = gic.take_interrupt() {
        let deliver = vm::console::service_interrupt();
        gic.finish_owned_interrupt(interrupt, deliver);
    }
}
