//! Fixed Erista identity map: pass through physical memory except resident EL2 RAM.
//!
//! Armv8.0, 36-bit IPA/PA, 4 KiB granules; split RAM and the trapped MC page.
//! This describes CPU access permissions, not DMA isolation or a hardware RAM audit.

use crate::{
    BLOCK_SIZE, IPA_LIMIT, PAGE_SIZE,
    drivers::interrupt::gicv2::Layout as GicLayout,
    mc,
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
    vdev::uart,
};

pub const GIB: u64 = 1 << 30;
pub const ENTRIES: usize = 512;
pub const RAM_BASE: u64 = 0x8000_0000;
pub const VTCR: u64 = (1 << 31) | (1 << 16) | (3 << 12) | (1 << 10) | (1 << 8) | (1 << 6) | 28;
// Route physical IRQs to EL2 for hardware-assisted GICv2 forwarding. FIQ and
// SError remain at EL1. Trap SMC for virtual CPU power state.
pub const HCR: u64 = (1 << 31) | (1 << 19) | (1 << 4) | 1;
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
    GicLayout,
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
        if address != mc::BASE && address != uart::BASE && address != crate::vdev::virtio_net::BASE
        {
            *descriptor = address | DEVICE | 2; // L3 page descriptor
        }
    }
    mmio.0[((mc::BASE % GIB) / BLOCK_SIZE) as usize] = addresses[3] | 3;
    root.0[MC_INDEX] = addresses[2] | 3;
    root.0[SPLIT_INDEX] = (addresses[1] & ADDRESS_MASK) | 3;
    Ok((VMID << 48) | addresses[0])
}

/// Apply USB ownership before any CPU enables this otherwise immutable map.
pub fn protect_usb(
    mmio: &mut Table,
    pages: &mut Table,
    car: &mut Table,
    address: u64,
    existing: &[u64],
) -> Result<(), MapError> {
    use crate::vdev::usb_ownership::{self as ownership, CAR, PMC_PAGE};
    if !resident_table(address) || existing.contains(&address) {
        return Err(MapError::TablePlacement);
    }
    for (index, entry) in pages.0.iter_mut().enumerate() {
        let ipa = (mc::BASE & !(BLOCK_SIZE - 1)) + index as u64 * PAGE_SIZE;
        if ownership::controller(ipa) || ipa == PMC_PAGE {
            *entry = 0;
        }
    }
    mmio.0[((0x7d00_0000 % GIB) / BLOCK_SIZE) as usize] = 0;
    let base = CAR & !(BLOCK_SIZE - 1);
    for (index, entry) in car.0.iter_mut().enumerate() {
        let ipa = base + index as u64 * PAGE_SIZE;
        *entry = if matches!(ipa, CAR | ownership::LIC) {
            0
        } else {
            ipa | DEVICE | 2
        };
    }
    mmio.0[((CAR % GIB) / BLOCK_SIZE) as usize] = address | 3;
    Ok(())
}

fn page_range(base: u64, size: u64) -> Option<core::ops::Range<u64>> {
    let end = base.checked_add(size)?;
    (base % PAGE_SIZE == 0
        && size != 0
        && size % PAGE_SIZE == 0
        && end <= IPA_LIMIT
        && base / BLOCK_SIZE == (end - 1) / BLOCK_SIZE)
        .then_some(base..end)
}

/// Trap the guest distributor, expose GICV at the guest's original GICC IPA,
/// and hide the physical GICH/GICV windows. Both extra tables live in VMM RAM.
pub fn protect_gic(
    root: &mut Table,
    mmio: &mut Table,
    gic_l2: &mut Table,
    gic_pages: &mut Table,
    addresses: [u64; 2],
    existing: &[u64],
    layout: GicLayout,
) -> Result<(), MapError> {
    for (index, &address) in addresses.iter().enumerate() {
        if !resident_table(address)
            || existing.contains(&address)
            || addresses[..index].contains(&address)
        {
            return Err(MapError::TablePlacement);
        }
    }
    let ranges = [
        page_range(layout.distributor, layout.distributor_size),
        page_range(layout.cpu, layout.cpu_size),
        page_range(layout.hypervisor, layout.hypervisor_size),
        page_range(layout.virtual_cpu, layout.virtual_cpu_size),
    ];
    let [
        Some(distributor),
        Some(cpu),
        Some(hypervisor),
        Some(virtual_cpu),
    ] = ranges
    else {
        return Err(MapError::GicLayout);
    };
    if layout.cpu_size > layout.virtual_cpu_size {
        return Err(MapError::GicLayout);
    }
    let block_base = layout.distributor & !(BLOCK_SIZE - 1);
    if [layout.cpu, layout.hypervisor, layout.virtual_cpu]
        .iter()
        .any(|base| *base & !(BLOCK_SIZE - 1) != block_base)
    {
        return Err(MapError::GicLayout);
    }
    let all = [&distributor, &cpu, &hypervisor, &virtual_cpu];
    for index in 0..all.len() {
        for other in &all[index + 1..] {
            if all[index].start < other.end && other.start < all[index].end {
                return Err(MapError::GicLayout);
            }
        }
    }

    let gib_index = (block_base / GIB) as usize;
    let block_index = ((block_base % GIB) / BLOCK_SIZE) as usize;
    if gib_index != MC_INDEX && (gib_index == SPLIT_INDEX || root.0[gib_index] & 3 != 1) {
        return Err(MapError::GicLayout);
    }

    gic_l2.0.fill(0);
    gic_pages.0.fill(0);
    for (index, descriptor) in gic_pages.0.iter_mut().enumerate() {
        let address = block_base + index as u64 * PAGE_SIZE;
        *descriptor = address | DEVICE | 2;
    }
    for address in distributor.step_by(PAGE_SIZE as usize) {
        gic_pages.0[((address - block_base) / PAGE_SIZE) as usize] = 0;
    }
    for offset in (0..layout.cpu_size).step_by(PAGE_SIZE as usize) {
        let ipa = layout.cpu + offset;
        gic_pages.0[((ipa - block_base) / PAGE_SIZE) as usize] =
            (layout.virtual_cpu + offset) | DEVICE | 2;
    }
    for range in [hypervisor, virtual_cpu] {
        for address in range.step_by(PAGE_SIZE as usize) {
            gic_pages.0[((address - block_base) / PAGE_SIZE) as usize] = 0;
        }
    }

    if gib_index == MC_INDEX {
        mmio.0[block_index] = addresses[1] | 3;
    } else {
        let gib_base = gib_index as u64 * GIB;
        for (index, descriptor) in gic_l2.0.iter_mut().enumerate() {
            *descriptor = (gib_base + index as u64 * BLOCK_SIZE) | DEVICE;
        }
        gic_l2.0[block_index] = addresses[1] | 3;
        root.0[gib_index] = addresses[0] | 3;
    }
    Ok(())
}
