use switchvisor_core::{
    BLOCK_SIZE, IPA_LIMIT, PAGE_SIZE, mc,
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
    stage2::{self, GIB, Table},
};

const ADDRESSES: [u64; 4] = [
    RESIDENT_BASE + 0x1000,
    RESIDENT_BASE + 0x2000,
    RESIDENT_BASE + 0x3000,
    RESIDENT_BASE + 0x4000,
];

fn translate(tables: &[Table; 4], ipa: u64) -> Option<(u64, u64)> {
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
                table = &tables[ADDRESSES
                    .iter()
                    .position(|&address| address == descriptor & 0x0000_000f_ffff_f000)?]
            }
            _ => return None,
        }
    }
    None
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
                (page != mc::BASE).then_some(ipa)
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
    assert_eq!(stage2::HCR & ((1 << 3) | (1 << 4) | (1 << 5)), 0);
    assert_eq!(stage2::HCR & 1, 1);
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
fn firmware_cannot_start_or_resume_an_unprotected_cpu() {
    for call in [
        0x8400_0001,
        0xc400_0001,
        0x8400_0003,
        0xc400_0003,
        0xc400_000e,
    ] {
        assert!(stage2::unsupported_psci(call));
    }
    for call in [0x8400_0000, 0x8400_0002, 0x8400_0008, 0x8400_0009] {
        assert!(!stage2::unsupported_psci(call));
    }
}
