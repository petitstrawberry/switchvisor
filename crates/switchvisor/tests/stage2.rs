use switchvisor::{
    BLOCK_SIZE, IPA_LIMIT, PAGE_SIZE,
    drivers::interrupt::gicv2::Layout as GicLayout,
    mc,
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
    stage2::{self, GIB, Table},
    vdev::uart,
};

const ADDRESSES: [u64; 4] = [
    RESIDENT_BASE + 0x1000,
    RESIDENT_BASE + 0x2000,
    RESIDENT_BASE + 0x3000,
    RESIDENT_BASE + 0x4000,
];

fn translate<const N: usize>(tables: &[Table; N], ipa: u64) -> Option<(u64, u64)> {
    if ipa >= IPA_LIMIT {
        return None;
    }
    let mut table = &tables[0];
    for size in [GIB, BLOCK_SIZE, PAGE_SIZE] {
        let descriptor = table.0[((ipa / size) % 512) as usize];
        match descriptor & 3 {
            1 if size != PAGE_SIZE => {
                return Some((
                    (descriptor & 0x0000_000f_ffff_f000 & !(size - 1)) | (ipa & (size - 1)),
                    descriptor,
                ));
            }
            3 if size == PAGE_SIZE => {
                return Some((
                    (descriptor & 0x0000_000f_ffff_f000) | (ipa & (size - 1)),
                    descriptor,
                ));
            }
            3 => {
                let addresses = [
                    ADDRESSES[0],
                    ADDRESSES[1],
                    ADDRESSES[2],
                    ADDRESSES[3],
                    RESIDENT_BASE + 0x5000,
                    RESIDENT_BASE + 0x6000,
                ];
                table = tables.get(
                    addresses
                        .iter()
                        .position(|&address| address == descriptor & 0x0000_000f_ffff_f000)?,
                )?;
            }
            _ => return None,
        }
    }
    None
}

#[test]
fn usb_profile_traps_controllers_car_and_pmc_without_hiding_other_devices() {
    use switchvisor::vdev::usb_ownership as ownership;
    let mut tables = core::array::from_fn::<_, 5, _>(|_| Table::zeroed());
    let [root, split, mmio, pages, car] = &mut tables;
    stage2::build(root, split, mmio, pages, ADDRESSES).unwrap();
    stage2::protect_usb(mmio, pages, car, RESIDENT_BASE + 0x5000, &ADDRESSES).unwrap();
    for (start, size) in [
        (0x60000000, BLOCK_SIZE),
        (0x70000000, BLOCK_SIZE),
        (0x7d000000, BLOCK_SIZE),
    ] {
        for address in (start..start + size).step_by(PAGE_SIZE as usize) {
            let trapped = address == mc::BASE
                || address == uart::BASE
                || address == ownership::CAR
                || address == ownership::PMC_PAGE
                || ownership::controller(address);
            assert_eq!(
                translate(&tables, address).map(|value| value.0),
                (!trapped).then_some(address),
                "{address:x}"
            );
        }
    }
    for address in [
        0x700b0000, 0x700f0000, 0x70000000, 0x60007000, 0xfebff000, 0xffc00000,
    ] {
        assert_eq!(
            translate(&tables, address).map(|value| value.0),
            Some(address)
        );
    }
    for address in (RESIDENT_BASE..RESIDENT_BASE + RESIDENT_SIZE).step_by(PAGE_SIZE as usize) {
        assert_eq!(translate(&tables, address), None);
    }
}

#[test]
fn usb_table_must_be_unique_and_resident_before_the_map_is_changed() {
    for address in [
        0,
        RESIDENT_BASE + 1,
        RESIDENT_BASE + RESIDENT_SIZE,
        ADDRESSES[2],
    ] {
        let mut tables = core::array::from_fn::<_, 3, _>(|_| Table([0x55; 512]));
        let [mmio, pages, car] = &mut tables;
        assert!(stage2::protect_usb(mmio, pages, car, address, &ADDRESSES).is_err());
        assert!(tables.iter().all(|table| table.0 == [0x55; 512]));
    }
}

#[test]
fn every_block_and_mc_page_is_identity_mapped_except_the_exclusions() {
    let mut tables = core::array::from_fn(|_| Table::zeroed());
    let [root, split, mmio, pages] = &mut tables;
    assert_eq!(
        stage2::build(root, split, mmio, pages, ADDRESSES).unwrap(),
        (1 << 48) | ADDRESSES[0]
    );
    for base in (0..IPA_LIMIT).step_by(BLOCK_SIZE as usize) {
        for ipa in [base, base + BLOCK_SIZE - 1] {
            let excluded = (RESIDENT_BASE..RESIDENT_BASE + RESIDENT_SIZE).contains(&ipa);
            assert_eq!(
                translate(&tables, ipa).map(|r| r.0),
                (!excluded).then_some(ipa),
                "{ipa:#x}"
            );
        }
    }
    let base = mc::BASE & !(BLOCK_SIZE - 1);
    for page in (base..base + BLOCK_SIZE).step_by(PAGE_SIZE as usize) {
        for ipa in [page, page + PAGE_SIZE - 1] {
            assert_eq!(
                translate(&tables, ipa).map(|r| r.0),
                (page != mc::BASE && page != uart::BASE).then_some(ipa)
            );
            if let Some((_, descriptor)) = translate(&tables, ipa) {
                assert_eq!(descriptor & (0xf << 2), 0);
                assert_ne!(descriptor & (1 << 54), 0);
            }
        }
    }
    assert_eq!(translate(&tables, IPA_LIMIT), None);
    assert!(tables[0].0[64..].iter().all(|entry| *entry == 0));
    assert_eq!(tables[0].0[0] & (0xf << 2), 0);
    assert_ne!(tables[0].0[0] & (1 << 54), 0);
    assert_eq!(tables[0].0[2] & (0xf << 2), 0xf << 2);
    assert_eq!(stage2::HCR & ((1 << 3) | (1 << 5)), 0);
    assert_eq!(stage2::HCR & (1 << 4), 1 << 4);
    assert_eq!(stage2::HCR & 1, 1);
}

fn gic_map(layout: GicLayout) -> [Table; 6] {
    let mut tables = core::array::from_fn(|_| Table::zeroed());
    let [root, split, mmio, pages, gic_l2, gic_pages] = &mut tables;
    stage2::build(root, split, mmio, pages, ADDRESSES).unwrap();
    stage2::protect_gic(
        root,
        mmio,
        gic_l2,
        gic_pages,
        [RESIDENT_BASE + 0x5000, RESIDENT_BASE + 0x6000],
        &ADDRESSES,
        layout,
    )
    .unwrap();
    tables
}

#[test]
fn gic_cpu_interface_is_replaced_and_control_interfaces_are_hidden() {
    for layout in [GicLayout::QEMU_VIRT, GicLayout::TEGRA210] {
        let tables = gic_map(layout);
        for offset in (0..layout.distributor_size).step_by(PAGE_SIZE as usize) {
            assert_eq!(translate(&tables, layout.distributor + offset), None);
        }
        for offset in (0..layout.cpu_size).step_by(PAGE_SIZE as usize) {
            assert_eq!(
                translate(&tables, layout.cpu + offset).map(|entry| entry.0),
                Some(layout.virtual_cpu + offset)
            );
        }
        for (base, size) in [
            (layout.hypervisor, layout.hypervisor_size),
            (layout.virtual_cpu, layout.virtual_cpu_size),
        ] {
            for offset in (0..size).step_by(PAGE_SIZE as usize) {
                assert_eq!(translate(&tables, base + offset), None);
            }
        }
        let block = layout.distributor & !(BLOCK_SIZE - 1);
        for address in [block, block + BLOCK_SIZE - PAGE_SIZE] {
            if !(layout.distributor..layout.distributor + layout.distributor_size)
                .contains(&address)
                && !(layout.cpu..layout.cpu + layout.cpu_size).contains(&address)
                && !(layout.hypervisor..layout.hypervisor + layout.hypervisor_size)
                    .contains(&address)
                && !(layout.virtual_cpu..layout.virtual_cpu + layout.virtual_cpu_size)
                    .contains(&address)
            {
                assert_eq!(
                    translate(&tables, address).map(|entry| entry.0),
                    Some(address)
                );
            }
        }
    }
}

#[test]
fn gic_map_rejects_invalid_layout_before_changing_tables() {
    let invalid = GicLayout {
        cpu: GicLayout::QEMU_VIRT.distributor,
        ..GicLayout::QEMU_VIRT
    };
    let mut tables = core::array::from_fn::<_, 4, _>(|_| Table([0x55; 512]));
    let [root, mmio, gic_l2, gic_pages] = &mut tables;
    assert_eq!(
        stage2::protect_gic(
            root,
            mmio,
            gic_l2,
            gic_pages,
            [RESIDENT_BASE + 0x5000, RESIDENT_BASE + 0x6000],
            &ADDRESSES,
            invalid,
        ),
        Err(stage2::MapError::GicLayout)
    );
    assert!(tables.iter().all(|table| table.0 == [0x55; 512]));
}

#[test]
fn all_table_addresses_are_validated_before_any_mutation() {
    for index in 0..4 {
        for invalid in [
            0,
            RESIDENT_BASE + 1,
            RESIDENT_BASE + RESIDENT_SIZE,
            u64::MAX - 4095,
            ADDRESSES[(index + 1) % 4],
        ] {
            let mut addresses = ADDRESSES;
            addresses[index] = invalid;
            let mut tables = core::array::from_fn(|_| Table([0x55; 512]));
            let [root, split, mmio, pages] = &mut tables;
            assert!(stage2::build(root, split, mmio, pages, addresses).is_err());
            assert!(tables.iter().all(|table| table.0 == [0x55; 512]));
        }
    }
}

#[test]
fn firmware_cannot_resume_an_unprotected_cpu() {
    for call in [0x8400_0001, 0xc400_0001, 0xc400_000e] {
        assert!(switchvisor::psci::suspend(call));
    }
    for call in [0x8400_0000, 0x8400_0002, 0x8400_0008, 0x8400_0009] {
        assert!(!switchvisor::psci::suspend(call));
    }
}
