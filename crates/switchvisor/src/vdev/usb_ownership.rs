//! CPU MMIO ownership for the EL2 USB 2.0 device path; no device DMA isolation.

pub use crate::drivers::usb::tegra210::{CAR, DEV_ASID, PMC};
pub const PMC_PAGE: u64 = PMC & !4095;
pub const CONTROLLERS: [(u64, u64); 3] = [
    (0x7009_0000, 0x10000),  // XUSB host and PADCTL.
    (0x700d_0000, 0x10000),  // XUDC, PCI and IPFS.
    (0x7d00_0000, 0x200000), // Legacy USB controllers and PHYs.
];

pub fn controller(address: u64) -> bool {
    CONTROLLERS
        .iter()
        .any(|&(base, size)| (base..base + size).contains(&address))
}

pub fn reads_current(address: u64) -> bool {
    address == PMC + 0xf0
        || address.checked_sub(CAR).is_some_and(|offset| {
            matches!(
                offset,
                0x04 | 0x08 | 0x0c | 0x10 | 0x14 | 0x18 | 0x35c | 0x364 | 0x298 | 0x2a4
            )
        })
}

/// SET/CLR writes retain their write-one semantics. RMWs preserve USB-owned bits.
/// None means a guest write is entirely suppressed.
pub fn write(address: u64, value: u32, current: u32) -> Option<u32> {
    if address == DEV_ASID {
        return None;
    }
    if address == PMC + 0x30 && value & 0x100 != 0 && matches!(value & 31, 20..=22) {
        return None;
    }
    if address == PMC + 0xf0 {
        return Some((value & !12) | (current & 12));
    }
    let Some(offset) = address.checked_sub(CAR).filter(|&off| off < 4096) else {
        return Some(value);
    };
    if matches!(
        offset,
        0xc0 | 0xc4 | 0xcc | 0x480 | 0x484 | 0x488 | 0x4c0 | 0x52c | 0x608 | 0x60c | 0x610 | 0x6cc
    ) {
        return None;
    }
    let (mask, write_one) = match offset {
        0x04 | 0x10 => (1 << 22, false),
        0x08 | 0x14 => ((1 << 26) | (1 << 27), false),
        0x0c | 0x18 => ((1 << 25) | (1 << 31), false),
        0x35c | 0x364 => ((1 << 14) | (1 << 15) | (1 << 28), false),
        0x298 | 0x2a4 => ((1 << 18) | (1 << 19), false),
        0x300 | 0x304 | 0x320 | 0x324 => (1 << 22, true),
        0x308 | 0x30c | 0x328 | 0x32c => ((1 << 26) | (1 << 27), true),
        0x310 | 0x314 | 0x330 | 0x334 => ((1 << 25) | (1 << 31), true),
        0x438 | 0x43c | 0x448 | 0x44c => ((1 << 14) | (1 << 15) | (1 << 28), true),
        0x29c | 0x2a0 | 0x2a8 | 0x2ac => ((1 << 18) | (1 << 19), true),
        _ => return Some(value),
    };
    Some(if write_one {
        value & !mask
    } else {
        (value & !mask) | (current & mask)
    })
}
