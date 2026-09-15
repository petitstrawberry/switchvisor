use switchvisor::memory::{AddressRange, RangeError};

#[test]
fn half_open_ranges_allow_adjacent_regions_and_reject_invalid_sizes() {
    let first = AddressRange::new(0x8000, 0x1000).unwrap();
    let next = AddressRange::new(0x9000, 0x1000).unwrap();

    assert!(!first.overlaps(next));
    assert!(first.overlaps(AddressRange::new(0x8fff, 2).unwrap()));
    assert_eq!(
        AddressRange::new(u64::MAX - 1, 2),
        Err(RangeError::Overflow)
    );
    assert_eq!(AddressRange::new(0, 0), Err(RangeError::Empty));
}
