use core::{arch::asm, cell::UnsafeCell};
use switchvisor_core::stage2::{self, Table};

// CPU0 owns construction; the tables are immutable after the EL1 handoff.
struct SharedTable(UnsafeCell<Table>);
unsafe impl Sync for SharedTable {}
static ROOT: SharedTable = SharedTable(UnsafeCell::new(Table::zeroed()));
static SPLIT: SharedTable = SharedTable(UnsafeCell::new(Table::zeroed()));
static MMIO: SharedTable = SharedTable(UnsafeCell::new(Table::zeroed()));
static PAGES: SharedTable = SharedTable(UnsafeCell::new(Table::zeroed()));

pub fn prepare() -> Result<(), stage2::MapError> {
    let root = ROOT.0.get();
    let split = SPLIT.0.get();
    let mmio = MMIO.0.get();
    let pages = PAGES.0.get();
    // EL2 is still identity-addressed with its MMU/D-cache off.
    unsafe {
        stage2::build(
            &mut *root,
            &mut *split,
            &mut *mmio,
            &mut *pages,
            [root as u64, split as u64, mmio as u64, pages as u64],
        )?;
    };
    Ok(())
}

/// CPU0 completed and published these immutable tables before any guest CPU_ON.
pub unsafe fn enable() {
    let vttbr = (stage2::VMID << 48) | ROOT.0.get() as u64;
    unsafe {
        asm!(
            "dsb sy",
            "msr vtcr_el2, {vtcr}",
            "msr vttbr_el2, {vttbr}",
            "isb",
            "tlbi vmalls12e1is",
            "dsb sy",
            "isb",
            vtcr = in(reg) stage2::VTCR,
            vttbr = in(reg) vttbr,
            options(nostack),
        );
    }
}
