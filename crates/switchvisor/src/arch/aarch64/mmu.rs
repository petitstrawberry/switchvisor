use core::{arch::asm, cell::UnsafeCell};
use switchvisor::{
    el2_mmu,
    stage2::{MapError, Table},
};

struct SharedTable(UnsafeCell<Table>);
// CPU0 constructs this before starting any other CPU. It is then immutable.
unsafe impl Sync for SharedTable {}
static ROOT: SharedTable = SharedTable(UnsafeCell::new(Table::zeroed()));
static SPLIT: SharedTable = SharedTable(UnsafeCell::new(Table::zeroed()));

pub fn prepare() -> Result<(), MapError> {
    let root = ROOT.0.get();
    let split = SPLIT.0.get();
    unsafe { el2_mmu::build(&mut *root, &mut *split, [root as u64, split as u64]) }
}

/// The immutable table was completed before firmware CPU_ON or the first handoff.
pub unsafe fn enable() {
    let sctlr: u64;
    unsafe {
        asm!("dsb sy", "msr mair_el2, {mair}", "msr tcr_el2, {tcr}",
            "msr ttbr0_el2, {root}", "isb", "tlbi alle2", "dsb sy", "isb",
            mair = in(reg) el2_mmu::MAIR, tcr = in(reg) el2_mmu::TCR,
            root = in(reg) ROOT.0.get() as u64, options(nostack));
        asm!("mrs {sctlr}, sctlr_el2", sctlr = out(reg) sctlr, options(nomem, nostack));
        // Only private monitor RAM is cacheable. The DMA block and guest aliases
        // stay NC; enabling C/I cannot allocate cache lines for either of them.
        asm!("ic iallu", "dsb sy", "isb", "msr sctlr_el2, {sctlr}", "isb",
            sctlr = in(reg) (sctlr | 1 | (1 << 2) | (1 << 12)), options(nostack));
    }
}
