//! Memory discovery for the Erista Hekate L4T profile. Hardware registers stay intact.
use crate::payload::{RESIDENT_BASE, RESIDENT_SIZE};

pub const BASE: u64 = 0x7001_9000;
pub const SIZE: u64 = 4096;
pub const EMEM_CFG: u64 = 0x50;
pub const GSC5_BOM: u64 = 0xd4c;
pub const GSC5_HI: u64 = 0xd50;
pub const GSC5_SIZE: u64 = 0xd54;

// BOM, high address, size. GSC registers are 0x50 bytes apart, not 0x14.
const CARVEOUTS: [(u64, u64, u64); 8] = [
    (0x670, 0x9d4, 0x674), // secure monitor
    (0x9a0, 0x9a8, 0x9a4), // MTS
    (0x648, 0x978, 0x64c), // VPR
    (0xc0c, 0xc10, 0xc14),
    (0xc5c, 0xc60, 0xc64),
    (0xcac, 0xcb0, 0xcb4),
    (0xcfc, 0xd00, 0xd04),
    (GSC5_BOM, GSC5_HI, GSC5_SIZE),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementError {
    RamSize,
    FirmwareCarveout,
    Gsc5InUse,
}

/// End of the DRAM aperture reported by the Tegra210 memory controller.
pub fn ram_end(mut read: impl FnMut(u64) -> u32) -> Result<u64, PlacementError> {
    let config = read(EMEM_CFG);
    let megabytes = if config & (1 << 31) != 0 {
        (config & 0x3fff)
            .checked_sub(0x800)
            .ok_or(PlacementError::RamSize)?
    } else {
        config
    };
    let bytes = u64::from(megabytes) << 20;
    if bytes == 0 || bytes > crate::IPA_LIMIT - 0x8000_0000 {
        return Err(PlacementError::RamSize);
    }
    Ok(0x8000_0000 + bytes)
}

/// Match the pinned U-Boot's RAM-size and lowest-carveout calculation below 4 GiB.
/// Called before relocation; this function uses no mutable globals or absolute pointers.
#[inline]
pub fn validate(mut read: impl FnMut(u64) -> u32) -> Result<(), PlacementError> {
    let mut top = ram_end(&mut read)?.min(0x1_0000_0000);
    for (bom, high, size) in CARVEOUTS {
        let address = u64::from(read(bom)) | (u64::from(read(high)) << 32);
        if read(size) != 0 && (0x8000_0000..=0x1_0000_0000).contains(&address) {
            top = top.min(address);
        }
    }
    if top < RESIDENT_BASE + RESIDENT_SIZE {
        return Err(PlacementError::FirmwareCarveout);
    }
    if read(GSC5_SIZE) != 0 {
        return Err(PlacementError::Gsc5InUse);
    }
    Ok(())
}

/// An otherwise disabled GSC5 advertises a read-only software reservation.
pub fn read_override(offset: u64) -> Option<u32> {
    match offset {
        GSC5_BOM => Some(RESIDENT_BASE as u32),
        GSC5_HI => Some((RESIDENT_BASE >> 32) as u32),
        GSC5_SIZE => Some((RESIDENT_SIZE >> 17) as u32),
        _ => None,
    }
}
