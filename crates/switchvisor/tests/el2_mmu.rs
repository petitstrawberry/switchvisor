use switchvisor::{
    IPA_LIMIT, el2_mmu,
    stage2::{GIB, Table},
};

#[test]
fn el2_ram_is_shareable_normal_non_cacheable_and_mmio_is_device_xn() {
    let mut root = Table::zeroed();
    el2_mmu::build(&mut root, 0xfec01000).unwrap();
    for index in 0..64 {
        let entry = root.0[index];
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
    assert_eq!(el2_mmu::MAIR, 0x4400);
    assert_eq!(el2_mmu::TCR, 0x8081301c);
    for invalid in [0, 0x70019000, 0xfec01001, IPA_LIMIT, u64::MAX] {
        let mut root = Table([55; 512]);
        assert!(el2_mmu::build(&mut root, invalid).is_err());
        assert_eq!(root.0, [55; 512]);
    }
}
