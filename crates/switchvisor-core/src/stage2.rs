//! Fixed Erista identity map: pass through physical memory except resident EL2 RAM.
//!
//! Armv8.0, 36-bit IPA/PA, 4 KiB granules; split RAM and the trapped MC page.
//! This describes CPU access permissions, not DMA isolation or a hardware RAM audit.

use crate::{
    BLOCK_SIZE, IPA_LIMIT, PAGE_SIZE, mc,
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
};

pub const GIB: u64 = 1 << 30;
pub const ENTRIES: usize = 512;
pub const RAM_BASE: u64 = 0x8000_0000;
pub const VTCR: u64 = (1 << 31) | (1 << 16) | (3 << 12) | (1 << 10) | (1 << 8) | (1 << 6) | 28;
// Physical IRQ/FIQ/SError remain at EL1. Trap SMC for virtual CPU power state
// and protected EL2 secondary entry; other native services are forwarded.
pub const HCR: u64 = (1 << 31) | (1 << 19) | 1;
pub const VMID: u64 = 1;
pub const SPLIT_INDEX: usize = (RESIDENT_BASE / GIB) as usize;
pub const MC_INDEX: usize = (mc::BASE / GIB) as usize;

const ADDRESS_MASK: u64 = 0x0000_000f_ffff_f000;
const ACCESS: u64 = (3 << 6) | (3 << 8) | (1 << 10); // RW, inner-shareable, AF.
const NORMAL: u64 = (0xf << 2) | ACCESS | 1;
const DEVICE: u64 = ACCESS | 1 | (1 << 54); // Device-nGnRnE, execute-never.

#[repr(C, align(4096))]
pub struct Table(pub [u64; ENTRIES]);

impl Table {
    pub const fn zeroed() -> Self {
        Self([0; ENTRIES])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    TablePlacement,
}

fn resident_table(address: u64) -> bool {
    address % PAGE_SIZE == 0
        && address >= RESIDENT_BASE
        && address
            .checked_add(PAGE_SIZE)
            .is_some_and(|end| end <= RESIDENT_BASE + RESIDENT_SIZE)
}

/// All four tables must physically reside in the excluded region. No heap is used.
pub fn build(
    root: &mut Table,
    split: &mut Table,
    mmio: &mut Table,
    pages: &mut Table,
    addresses: [u64; 4],
) -> Result<u64, MapError> {
    for (index, &address) in addresses.iter().enumerate() {
        if !resident_table(address) || addresses[..index].contains(&address) {
            return Err(MapError::TablePlacement);
        }
    }
    root.0.fill(0);
    split.0.fill(0);
    mmio.0.fill(0);
    pages.0.fill(0);
    for index in 0..(IPA_LIMIT / GIB) as usize {
        let address = index as u64 * GIB;
        root.0[index] = address | if address < RAM_BASE { DEVICE } else { NORMAL };
    }
    let base = SPLIT_INDEX as u64 * GIB;
    for (index, descriptor) in split.0.iter_mut().enumerate() {
        let address = base + index as u64 * BLOCK_SIZE;
        if !(RESIDENT_BASE..RESIDENT_BASE + RESIDENT_SIZE).contains(&address) {
            *descriptor = address | NORMAL;
        }
    }
    let mmio_base = MC_INDEX as u64 * GIB;
    for (index, descriptor) in mmio.0.iter_mut().enumerate() {
        *descriptor = (mmio_base + index as u64 * BLOCK_SIZE) | DEVICE;
    }
    let page_base = mc::BASE & !(BLOCK_SIZE - 1);
    for (index, descriptor) in pages.0.iter_mut().enumerate() {
        let address = page_base + index as u64 * PAGE_SIZE;
        if address != mc::BASE {
            *descriptor = address | DEVICE | 2; // L3 page descriptor
        }
    }
    mmio.0[((mc::BASE % GIB) / BLOCK_SIZE) as usize] = addresses[3] | 3;
    root.0[MC_INDEX] = addresses[2] | 3;
    root.0[SPLIT_INDEX] = (addresses[1] & ADDRESS_MASK) | 3;
    Ok((VMID << 48) | addresses[0])
}
