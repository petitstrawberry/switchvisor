use switchvisor_core::payload::{
    CONFIG_OFFSET, CONFIG_SIZE, MAX_PACKAGE_SIZE, MAX_RUNTIME_SIZE, Payload, PayloadError, crc32,
};

fn fixture() -> (Payload, Vec<u8>) {
    let mut package = vec![0u8; 0x4000];
    // Deliberately opaque raw AArch64 bytes: no U-Boot or Linux Image header.
    package.extend_from_slice(&[0x1f, 0x20, 0x03, 0xd5, 0, 0, 0, 0]);
    let payload = Payload {
        package_size: package.len() as u64,
        bootstrap_size: 0x3000,
        offset: 0x4000,
        file_size: 8,
        runtime_size: 0x10000,
        entry_offset: 0,
        registers: [0; 8],
        preserve_boot_args: true,
        crc32: crc32(&package[0x4000..]),
    };
    (payload, package)
}

#[test]
fn opaque_raw_data_and_both_handoff_modes_are_supported() {
    let (mut payload, package) = fixture();
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(payload.source(&package).unwrap(), &package[0x4000..]);
    assert_eq!(
        Payload::decode(&payload.encode().unwrap()).unwrap(),
        Some(payload)
    );
    assert_eq!(Payload::decode(&[0; CONFIG_SIZE]).unwrap(), None);
    payload.preserve_boot_args = false;
    payload.registers = [1, 2, 3, 4, 5, 6, 7, u64::MAX];
    payload.entry_offset = 4;
    assert_eq!(payload.entry().unwrap(), 0xaa00_0004);
    assert_eq!(
        Payload::decode(&payload.encode().unwrap()).unwrap(),
        Some(payload)
    );
    payload.preserve_boot_args = true;
    assert_eq!(payload.validate(), Err(PayloadError::Header));
}

#[test]
fn runtime_tail_budget_and_entry_bounds_are_checked() {
    let (original, _) = fixture();
    for size in [0, 7, MAX_RUNTIME_SIZE + 1, u64::MAX] {
        let mut payload = original;
        payload.runtime_size = size;
        assert_eq!(payload.validate(), Err(PayloadError::RuntimeRange));
    }
    for entry in [1, 6, 8, u64::MAX, u64::MAX - 3] {
        let mut payload = original;
        payload.entry_offset = entry;
        assert_eq!(payload.validate(), Err(PayloadError::Entry));
    }
}

#[test]
fn package_offsets_and_overflow_cannot_escape_the_source_window() {
    let (original, _) = fixture();
    for (offset, size) in [
        (u64::MAX - 7, 8),
        (0x1000, 8),
        (0x4001, 8),
        (MAX_PACKAGE_SIZE, 8),
    ] {
        let mut payload = original;
        payload.offset = offset;
        payload.file_size = size;
        payload.package_size = offset.wrapping_add(size);
        assert_eq!(payload.validate(), Err(PayloadError::PackageRange));
    }
    let mut payload = original;
    payload.bootstrap_size = (CONFIG_OFFSET + CONFIG_SIZE - 1) as u64;
    assert_eq!(payload.validate(), Err(PayloadError::PackageRange));
    payload.bootstrap_size = original.bootstrap_size + 1;
    assert_eq!(payload.validate(), Err(PayloadError::PackageRange));
}

#[test]
fn corrupted_and_truncated_input_is_rejected_before_a_copy() {
    let (payload, package) = fixture();
    let encoded = payload.encode().unwrap();
    for end in 0..CONFIG_SIZE {
        assert!(Payload::decode(&encoded[..end]).is_err());
    }
    for i in 0..CONFIG_SIZE {
        let mut modified = encoded;
        modified[i] ^= 1;
        assert!(Payload::decode(&modified).is_err());
    }
    for end in 0..package.len() {
        assert!(payload.source(&package[..end]).is_err());
    }
    let mut corrupted = package;
    *corrupted.last_mut().unwrap() ^= 1;
    assert_eq!(payload.source(&corrupted), Err(PayloadError::Checksum));
}
