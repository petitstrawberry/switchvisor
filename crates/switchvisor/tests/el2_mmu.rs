use switchvisor::{
    BLOCK_SIZE, IPA_LIMIT, el2_mmu,
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
    stage2::{GIB, Table},
};

#[test]
fn only_private_monitor_ram_is_cacheable_and_dma_is_nc_xn() {
    let mut root = Table::zeroed();
    let mut split = Table::zeroed();
    el2_mmu::build(&mut root, &mut split, [0xfec01000, 0xfec02000]).unwrap();
    for index in 0..64 {
        let entry = root.0[index];
        if index == (RESIDENT_BASE / GIB) as usize {
            assert_eq!(entry, 0xfec02003);
            continue;
        }
        assert_eq!(entry & 0x0000000fc0000000, index as u64 * GIB);
        assert_eq!(entry & 3, 1);
        assert_ne!(entry & (1 << 10), 0);
        if index < 2 {
            assert_eq!(entry & (7 << 2), 0);
            assert_ne!(entry & (1 << 54), 0);
        } else {
            assert_eq!(entry & (7 << 2), 1 << 2);
            assert_eq!(entry & (3 << 8), 3 << 8);
            assert_eq!(entry & (1 << 54), 0);
        }
    }
    assert!(root.0[64..].iter().all(|entry| *entry == 0));
    for (index, entry) in split.0.iter().copied().enumerate() {
        let address = (RESIDENT_BASE & !(GIB - 1)) + index as u64 * BLOCK_SIZE;
        assert_eq!(entry & 0x0000000fffe00000, address);
        assert_eq!(entry & 3, 1);
        assert_eq!(entry & (3 << 8), 3 << 8);
        assert_ne!(entry & (1 << 10), 0);
        let private = (RESIDENT_BASE..RESIDENT_BASE + RESIDENT_SIZE).contains(&address);
        let dma = address == el2_mmu::DMA_BASE;
        assert_eq!(
            entry & (7 << 2),
            if private && !dma { 2 << 2 } else { 1 << 2 }
        );
        assert_eq!(entry & (1 << 54) != 0, dma);
    }
    assert_eq!(el2_mmu::MAIR, 0xff4400);
    assert_eq!(el2_mmu::TCR, 0x8081351c);
}

#[test]
fn invalid_or_aliased_tables_never_partially_modify_the_map() {
    for invalid in [
        0,
        0x70019000,
        0xfec01001,
        0xfec02000,
        el2_mmu::DMA_BASE,
        el2_mmu::DMA_END - 4096,
        RESIDENT_BASE + RESIDENT_SIZE,
        IPA_LIMIT,
        u64::MAX,
    ] {
        let mut root = Table([55; 512]);
        let mut split = Table([66; 512]);
        assert!(el2_mmu::build(&mut root, &mut split, [invalid, 0xfec02000]).is_err());
        assert_eq!(root.0, [55; 512]);
        assert_eq!(split.0, [66; 512]);
    }
}
