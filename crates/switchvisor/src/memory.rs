use core::fmt;

use crate::{BLOCK_SIZE, HV_SIZE_BUDGET, IPA_LIMIT, PAGE_SIZE};

/// A nonempty half-open physical range. Construction rejects address overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressRange {
    start: u64,
    end: u64,
}

impl AddressRange {
    pub fn new(start: u64, size: u64) -> Result<Self, RangeError> {
        if size == 0 {
            return Err(RangeError::Empty);
        }
        let end = start.checked_add(size).ok_or(RangeError::Overflow)?;
        Ok(Self { start, end })
    }

    pub const fn start(self) -> u64 {
        self.start
    }
    pub const fn end(self) -> u64 {
        self.end
    }
    pub const fn size(self) -> u64 {
        self.end - self.start
    }
    pub const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
    pub const fn contains(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
    pub const fn aligned(self, alignment: u64) -> bool {
        alignment != 0 && self.start % alignment == 0 && self.size() % alignment == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    Empty,
    Overflow,
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty address range",
            Self::Overflow => "address range overflows u64",
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NamedRegion<'a> {
    pub name: &'a str,
    pub range: AddressRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestRegionKind {
    ScarletRuntime,
    OsDtb,
    Initramfs,
    Uimage,
    PlatformDtimg,
    UbootImage,
    UbootInitialRam,
    UbootRelocation,
    BootScript,
}

impl GuestRegionKind {
    pub const REQUIRED: [Self; 9] = [
        Self::ScarletRuntime,
        Self::OsDtb,
        Self::Initramfs,
        Self::Uimage,
        Self::PlatformDtimg,
        Self::UbootImage,
        Self::UbootInitialRam,
        Self::UbootRelocation,
        Self::BootScript,
    ];

    pub const fn fixed_base(self) -> Option<u64> {
        match self {
            Self::ScarletRuntime => Some(crate::SCARLET_LOAD_BASE),
            Self::OsDtb => Some(0x8d00_0000),
            Self::Initramfs => Some(0x9200_0000),
            Self::Uimage => Some(0xa000_0000),
            Self::PlatformDtimg => Some(0xa800_0000),
            Self::UbootImage => Some(crate::BL33_LOAD_BASE),
            Self::BootScript => Some(0x8fe0_0000),
            Self::UbootInitialRam | Self::UbootRelocation => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GuestRegion<'a> {
    pub kind: GuestRegionKind,
    pub region: NamedRegion<'a>,
}

/// Physical geometry only; verification flags describe the supplied profile's provenance.
/// This is not DMA isolation or evidence of a successful hardware boot.
pub struct BootProfile<'a> {
    pub sku: u32,
    pub memory_map_verified: bool,
    pub uboot_layout_verified: bool,
    pub hv_region: Option<AddressRange>,
    pub usb_dma_region: Option<AddressRange>,
    pub usable_banks: &'a [AddressRange],
    pub protected_regions: &'a [NamedRegion<'a>],
    pub guest_regions: &'a [GuestRegion<'a>],
}

const FRAMEBUFFER: AddressRange = AddressRange {
    start: 0xf5a0_0000,
    end: 0xf5e0_0000,
};
// Preserve the full Hekate environment window, including the preceding magic word.
const HEKATE_ENVIRONMENT: AddressRange = AddressRange {
    start: 0xa9fb_fffc,
    end: 0xaa00_0000,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileError {
    UnsupportedSku,
    UnresolvedHypervisor,
    UnresolvedUsbDma,
    UnverifiedMemoryMap,
    UnverifiedUbootLayout,
    NoUsableBanks,
    InvalidBank(usize),
    OverlappingBanks(usize, usize),
    HypervisorAlignment,
    HypervisorBudget,
    HypervisorOutsideRam,
    UsbDmaPlacement,
    ProtectedConflict(&'static str, usize),
    FixedReservationConflict(&'static str, &'static str),
    MissingGuestRegion(GuestRegionKind),
    DuplicateGuestRegion(GuestRegionKind),
    InvalidGuestRegion(usize),
    GuestOverlap(usize, usize),
    GuestHypervisorConflict(usize),
    GuestProtectedConflict(usize, usize),
    GuestFixedReservationConflict(usize, &'static str),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl BootProfile<'_> {
    pub fn validate(&self) -> Result<(), ProfileError> {
        use ProfileError::*;
        if self.sku != 0 {
            return Err(UnsupportedSku);
        }
        let hv = self.hv_region.ok_or(UnresolvedHypervisor)?;
        let dma = self.usb_dma_region.ok_or(UnresolvedUsbDma)?;
        if !self.memory_map_verified {
            return Err(UnverifiedMemoryMap);
        }
        if !self.uboot_layout_verified {
            return Err(UnverifiedUbootLayout);
        }
        if self.usable_banks.is_empty() {
            return Err(NoUsableBanks);
        }
        for (i, bank) in self.usable_banks.iter().enumerate() {
            if !bank.aligned(PAGE_SIZE) || bank.end() > IPA_LIMIT {
                return Err(InvalidBank(i));
            }
            for (j, previous) in self.usable_banks[..i].iter().enumerate() {
                if bank.overlaps(*previous) {
                    return Err(OverlappingBanks(j, i));
                }
            }
        }
        let in_ram = |r| self.usable_banks.iter().any(|bank| bank.contains(r));
        if !hv.aligned(BLOCK_SIZE) {
            return Err(HypervisorAlignment);
        }
        if hv.size() < HV_SIZE_BUDGET {
            return Err(HypervisorBudget);
        }
        if !in_ram(hv) {
            return Err(HypervisorOutsideRam);
        }
        if !dma.aligned(PAGE_SIZE) || !hv.contains(dma) || dma.end() > (1 << 32) {
            return Err(UsbDmaPlacement);
        }
        for (name, reserved) in [
            ("framebuffer", FRAMEBUFFER),
            ("Hekate environment", HEKATE_ENVIRONMENT),
        ] {
            if hv.overlaps(reserved) {
                return Err(FixedReservationConflict("hypervisor", name));
            }
        }
        for (i, protected) in self.protected_regions.iter().enumerate() {
            if hv.overlaps(protected.range) {
                return Err(ProtectedConflict("hypervisor", i));
            }
        }
        for kind in GuestRegionKind::REQUIRED {
            match self.guest_regions.iter().filter(|r| r.kind == kind).count() {
                0 => return Err(MissingGuestRegion(kind)),
                1 => (),
                _ => return Err(DuplicateGuestRegion(kind)),
            }
        }
        for (i, guest) in self.guest_regions.iter().enumerate() {
            let r = guest.region.range;
            if !in_ram(r)
                || guest
                    .kind
                    .fixed_base()
                    .is_some_and(|base| r.start() != base)
            {
                return Err(InvalidGuestRegion(i));
            }
            if r.overlaps(hv) {
                return Err(GuestHypervisorConflict(i));
            }
            for (j, protected) in self.protected_regions.iter().enumerate() {
                if r.overlaps(protected.range) {
                    return Err(GuestProtectedConflict(i, j));
                }
            }
            for (name, reserved) in [
                ("framebuffer", FRAMEBUFFER),
                ("Hekate environment", HEKATE_ENVIRONMENT),
            ] {
                if r.overlaps(reserved) {
                    return Err(GuestFixedReservationConflict(i, name));
                }
            }
            for (j, previous) in self.guest_regions[..i].iter().enumerate() {
                if r.overlaps(previous.region.range) {
                    return Err(GuestOverlap(j, i));
                }
            }
        }
        Ok(())
    }
}
