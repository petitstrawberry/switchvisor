use switchvisor_core::{
    HV_SIZE_BUDGET,
    memory::{
        AddressRange, BootProfile, GuestRegion, GuestRegionKind as Kind, NamedRegion,
        ProfileError as Error, RangeError,
    },
};

fn range(base: u64, size: u64) -> AddressRange {
    AddressRange::new(base, size).unwrap()
}

// Synthetic geometry: these addresses are deliberately not a hardware boot profile.
struct Fixture {
    banks: Vec<AddressRange>,
    protected: Vec<NamedRegion<'static>>,
    guest: Vec<GuestRegion<'static>>,
    hv: AddressRange,
    dma: AddressRange,
}

impl Fixture {
    fn new() -> Self {
        let guest = [
            (Kind::ScarletRuntime, 0x8020_0000, 0x110_0000),
            (Kind::OsDtb, 0x8d00_0000, 0x4_0000),
            (Kind::Initramfs, 0x9200_0000, 0x100_0000),
            (Kind::Uimage, 0xa000_0000, 0x200_0000),
            (Kind::PlatformDtimg, 0xa800_0000, 0x40_0000),
            (Kind::UbootImage, 0xaa00_0000, 0x6_8200),
            (Kind::UbootInitialRam, 0x8a80_0000, 0x40_0000),
            (Kind::UbootRelocation, 0xe000_0000, 0x100_0000),
            (Kind::BootScript, 0x8fe0_0000, 0x1_0000),
        ]
        .into_iter()
        .map(|(kind, base, size)| GuestRegion {
            kind,
            region: NamedRegion {
                name: "synthetic guest region",
                range: range(base, size),
            },
        })
        .collect();
        Self {
            banks: vec![
                range(0x8000_0000, 0x7000_0000),
                range(0x1_0000_0000, 0x8000_0000),
            ],
            protected: vec![NamedRegion {
                name: "synthetic firmware",
                range: range(0xe800_0000, 0x80_0000),
            }],
            guest,
            hv: range(0xb000_0000, HV_SIZE_BUDGET),
            dma: range(0xb001_0000, 0x10_0000),
        }
    }

    fn profile(&self) -> BootProfile<'_> {
        BootProfile {
            sku: 0,
            memory_map_verified: true,
            uboot_layout_verified: true,
            hv_region: Some(self.hv),
            usb_dma_region: Some(self.dma),
            usable_banks: &self.banks,
            protected_regions: &self.protected,
            guest_regions: &self.guest,
        }
    }

    fn move_hv(&mut self, base: u64) {
        self.hv = range(base, HV_SIZE_BUDGET);
        self.dma = range(base + 0x1_0000, 0x10_0000);
    }
}

#[test]
fn half_open_ranges_allow_adjacent_regions_and_reject_overflow() {
    let first = range(0x8000, 0x1000);
    let next = range(0x9000, 0x1000);
    assert!(!first.overlaps(next));
    assert!(first.overlaps(range(0x8fff, 2)));
    assert_eq!(
        AddressRange::new(u64::MAX - 1, 2),
        Err(RangeError::Overflow)
    );
    assert_eq!(AddressRange::new(0, 0), Err(RangeError::Empty));
}

#[test]
fn complete_synthetic_profile_passes() {
    assert_eq!(Fixture::new().profile().validate(), Ok(()));
}

#[test]
fn unresolved_and_unverified_profiles_are_rejected() {
    let fixture = Fixture::new();
    let mut profile = fixture.profile();
    profile.hv_region = None;
    assert_eq!(profile.validate(), Err(Error::UnresolvedHypervisor));
    profile.hv_region = Some(fixture.hv);
    profile.usb_dma_region = None;
    assert_eq!(profile.validate(), Err(Error::UnresolvedUsbDma));
    profile.usb_dma_region = Some(fixture.dma);
    profile.memory_map_verified = false;
    assert_eq!(profile.validate(), Err(Error::UnverifiedMemoryMap));
    profile.memory_map_verified = true;
    profile.uboot_layout_verified = false;
    assert_eq!(profile.validate(), Err(Error::UnverifiedUbootLayout));
}

#[test]
fn sku_alignment_and_resident_budget_are_checked() {
    let fixture = Fixture::new();
    let mut profile = fixture.profile();
    profile.sku = 1;
    assert_eq!(profile.validate(), Err(Error::UnsupportedSku));
    profile.sku = 0;
    profile.hv_region = Some(range(0xb000_1000, HV_SIZE_BUDGET));
    assert_eq!(profile.validate(), Err(Error::HypervisorAlignment));
    profile.hv_region = Some(range(0xb000_0000, 0x20_0000));
    assert_eq!(profile.validate(), Err(Error::HypervisorBudget));
}

#[test]
fn runtime_bss_conflict_is_rejected_even_past_the_file_extent() {
    let mut fixture = Fixture::new();
    // A hypothetical 8 MiB file ends at 0x80a00000, but its runtime reaches 0x81300000.
    fixture.move_hv(0x80c0_0000);
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::GuestHypervisorConflict(0))
    );
}

#[test]
fn initial_stack_and_relocation_conflicts_are_rejected() {
    let mut fixture = Fixture::new();
    fixture.move_hv(0x8a80_0000);
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::GuestHypervisorConflict(6))
    );
    fixture.move_hv(0xe000_0000);
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::GuestHypervisorConflict(7))
    );
}

#[test]
fn firmware_framebuffer_and_environment_are_excluded() {
    let mut fixture = Fixture::new();
    fixture.move_hv(0xe800_0000);
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::ProtectedConflict("hypervisor", 0))
    );
    fixture.move_hv(0xa900_0000);
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::FixedReservationConflict(
            "hypervisor",
            "Hekate environment"
        ))
    );
    fixture.banks[0] = range(0x8000_0000, 0x8000_0000);
    fixture.move_hv(0xf500_0000);
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::FixedReservationConflict("hypervisor", "framebuffer"))
    );
}

#[test]
fn usb_dma_requires_page_alignment_ownership_and_32_bit_addresses() {
    let mut fixture = Fixture::new();
    fixture.dma = range(0xb001_0001, 0x10_0000);
    assert_eq!(fixture.profile().validate(), Err(Error::UsbDmaPlacement));
    fixture.dma = range(0xc000_0000, 0x10_0000);
    assert_eq!(fixture.profile().validate(), Err(Error::UsbDmaPlacement));
    fixture.move_hv(0x1_1000_0000);
    assert_eq!(fixture.profile().validate(), Err(Error::UsbDmaPlacement));
}

#[test]
fn banks_cannot_overlap_exceed_ipa_or_leave_a_hole_under_the_vmm() {
    let mut fixture = Fixture::new();
    fixture.banks.push(range(0x9000_0000, 0x100_0000));
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::OverlappingBanks(0, 2))
    );
    fixture.banks.pop();
    fixture.banks.push(range(1 << 36, 0x1000));
    assert_eq!(fixture.profile().validate(), Err(Error::InvalidBank(2)));
    fixture.banks = vec![
        range(0x8000_0000, 0x3000_0000),
        range(0xb080_0000, 0x3000_0000),
    ];
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::HypervisorOutsideRam)
    );
}

#[test]
fn required_guest_regions_cannot_be_omitted_duplicated_or_moved() {
    let mut fixture = Fixture::new();
    let script = fixture.guest.pop().unwrap();
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::MissingGuestRegion(Kind::BootScript))
    );
    fixture.guest.push(script);
    fixture.guest.push(script);
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::DuplicateGuestRegion(Kind::BootScript))
    );
    fixture.guest.pop();
    fixture.guest[0].region.range = range(0x8040_0000, 0x100_0000);
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::InvalidGuestRegion(0))
    );
}

#[test]
fn guest_buffers_cannot_overlap_or_touch_protected_memory() {
    let mut fixture = Fixture::new();
    fixture.guest[6].region.range = range(0x8020_0000, 0x1000);
    assert_eq!(fixture.profile().validate(), Err(Error::GuestOverlap(0, 6)));
    fixture.guest[6].region.range = fixture.protected[0].range;
    assert_eq!(
        fixture.profile().validate(),
        Err(Error::GuestProtectedConflict(6, 0))
    );
}
