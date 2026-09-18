use core::{arch::asm, cell::UnsafeCell};
use switchvisor::{
    el2_mmu,
    stage2::{MapError, Table},
};

struct SharedTable(UnsafeCell<Table>);
// CPU0 constructs this before starting any other CPU. It is then immutable.
unsafe impl Sync for SharedTable {}
static ROOT: SharedTable = SharedTable(UnsafeCell::new(Table::zeroed()));

pub fn prepare() -> Result<(), MapError> {
    let root = ROOT.0.get();
    unsafe { el2_mmu::build(&mut *root, root as u64) }
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
        // Keep EL2 caches disabled; the entire RAM map is Normal non-cacheable.
        asm!("msr sctlr_el2, {sctlr}", "isb",
            sctlr = in(reg) ((sctlr | 1) & !((1 << 2) | (1 << 12))), options(nostack));
    }
}

/// Translate an EL1 virtual address through the current guest's stage 1 and
/// stage 2 tables. The caller must validate the resulting physical address.
pub fn guest_physical(virtual_address: u64) -> Option<u64> {
    let saved_par: u64;
    let translated: u64;
    unsafe {
        asm!("mrs {saved}, par_el1", saved = out(reg) saved_par, options(nomem, nostack));
        asm!(
            "at s12e1r, {address}",
            "isb",
            "mrs {translated}, par_el1",
            address = in(reg) virtual_address,
            translated = out(reg) translated,
            options(nostack),
        );
        asm!("msr par_el1, {saved}", saved = in(reg) saved_par, options(nomem, nostack));
    }
    (translated & 1 == 0)
        .then_some((translated & 0x0000_ffff_ffff_f000) | (virtual_address & 0xfff))
}
