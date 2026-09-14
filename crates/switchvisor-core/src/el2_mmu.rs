//! Static 36-bit EL2 identity map. Normal non-cacheable RAM permits shared atomics
//! and DMA without a cacheable EL2 alias. MMIO stays Device-nGnRnE and XN.
use crate::{
    IPA_LIMIT, PAGE_SIZE,
    stage2::{GIB, RAM_BASE, Table},
};
pub const TCR: u64 = (1 << 31) | (1 << 23) | (1 << 16) | (3 << 12) | 28;
pub const MAIR: u64 = 0x4400; // Attr0: Device-nGnRnE; Attr1: Normal non-cacheable.

pub fn build(root: &mut Table, address: u64) -> Result<(), crate::stage2::MapError> {
    if address < RAM_BASE
        || address % PAGE_SIZE != 0
        || address
            .checked_add(PAGE_SIZE)
            .is_none_or(|end| end > IPA_LIMIT)
    {
        return Err(crate::stage2::MapError::TablePlacement);
    }
    root.0.fill(0);
    for index in 0..(IPA_LIMIT / GIB) as usize {
        let base = index as u64 * GIB;
        let attributes = if base < RAM_BASE {
            1 << 54
        } else {
            (1 << 2) | (3 << 8)
        };
        root.0[index] = base | attributes | (1 << 10) | 1;
    }
    Ok(())
}
