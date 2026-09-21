//! EL2 identity map: cache the monitor's private RAM, never its DMA window.
//! Guest RAM retains its non-cacheable alias and explicit cache maintenance;
//! firmware, framebuffer and guests need not agree on a cacheable RAM mapping.
use crate::{
    BLOCK_SIZE, IPA_LIMIT, PAGE_SIZE,
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
    stage2::{GIB, MapError, RAM_BASE, Table},
};

// Inner-shareable, write-back/read-allocate/write-allocate table walks.
pub const TCR: u64 = (1 << 31) | (1 << 23) | (1 << 16) | (3 << 12) | (1 << 10) | (1 << 8) | 28;
pub const MAIR: u64 = 0xff4400; // Device-nGnRnE, Normal NC, Normal WB RA/WA.
// The linker puts USB DMA in this separate 2 MiB block, outside the bootstrap.
pub const DMA_BASE: u64 = RESIDENT_BASE + BLOCK_SIZE;
pub const DMA_END: u64 = DMA_BASE + BLOCK_SIZE;
const NORMAL: u64 = (1 << 2) | (3 << 8) | (1 << 10) | 1;
const CACHED: u64 = (2 << 2) | (3 << 8) | (1 << 10) | 1;
const DEVICE: u64 = (1 << 54) | (1 << 10) | 1;

pub fn build(root: &mut Table, split: &mut Table, addresses: [u64; 2]) -> Result<(), MapError> {
    for (index, &address) in addresses.iter().enumerate() {
        if address < RESIDENT_BASE
            || address % PAGE_SIZE != 0
            || address.checked_add(PAGE_SIZE).is_none_or(|end| {
                end > RESIDENT_BASE + RESIDENT_SIZE || (address < DMA_END && end > DMA_BASE)
            })
            || addresses[..index].contains(&address)
        {
            return Err(MapError::TablePlacement);
        }
    }
    root.0.fill(0);
    for index in 0..(IPA_LIMIT / GIB) as usize {
        let base = index as u64 * GIB;
        root.0[index] = base | if base < RAM_BASE { DEVICE } else { NORMAL };
    }
    let base = RESIDENT_BASE & !(GIB - 1);
    for (index, entry) in split.0.iter_mut().enumerate() {
        let address = base + index as u64 * BLOCK_SIZE;
        let attributes = if address == DMA_BASE {
            NORMAL | (1 << 54)
        } else if (RESIDENT_BASE..RESIDENT_BASE + RESIDENT_SIZE).contains(&address) {
            CACHED
        } else {
            NORMAL
        };
        *entry = address | attributes;
    }
    root.0[(RESIDENT_BASE / GIB) as usize] = addresses[1] | 3;
    Ok(())
}
