use switchvisor_core::{
    fdt::{Fdt, FdtError},
    image::{ImageError, ScarletImage, UbootImage},
};

fn word(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}
fn put_word(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
fn put_quad(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn begin(bytes: &mut Vec<u8>, name: &str) {
    word(bytes, 1);
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
}

fn property_at(bytes: &mut Vec<u8>, offset: usize, name_offset: u32, value: &[u8]) {
    assert!(offset >= bytes.len() + 12);
    while bytes.len() + 12 < offset {
        word(bytes, 4);
    }
    assert_eq!(bytes.len() + 12, offset);
    word(bytes, 3);
    word(bytes, value.len() as u32);
    word(bytes, name_offset);
    bytes.extend_from_slice(value);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
}

// A structurally real, synthetic FDT with Hekate's exact property offsets.
fn fdt(status_shift: usize) -> Vec<u8> {
    let mut bytes = vec![0; 56];
    begin(&mut bytes, "");
    for (i, name) in ["serial@70006000", "serial@70006040", "serial@70006200"]
        .iter()
        .enumerate()
    {
        begin(&mut bytes, name);
        property_at(
            &mut bytes,
            0x1c94 + i * 0x12c + status_shift,
            0,
            b"disabled\0",
        );
        word(&mut bytes, 2);
    }
    begin(&mut bytes, "chosen");
    property_at(&mut bytes, 0x3f2c, 7, b"/serial@00000000\0");
    property_at(&mut bytes, 0x3f4c, 19, b"/serial@00000000\0");
    word(&mut bytes, 2);
    word(&mut bytes, 2);
    word(&mut bytes, 9);
    let strings = bytes.len();
    bytes.extend_from_slice(b"status\0stdout-path\0stderr-path\0");
    let size = bytes.len() as u32;
    for (offset, value) in [
        (0, 0xd00dfeed),
        (4, size),
        (8, 56),
        (12, strings as u32),
        (16, 40),
        (20, 17),
        (24, 16),
        (32, size - strings as u32),
        (36, strings as u32 - 56),
    ] {
        put_word(&mut bytes, offset, value);
    }
    bytes
}

fn uboot(status_shift: usize) -> Vec<u8> {
    let mut bytes = vec![0; 128];
    bytes[..4].copy_from_slice(&0x1400_0020u32.to_le_bytes());
    bytes.extend_from_slice(&fdt(status_shift));
    let runtime_size = bytes.len() as u64 + 4096;
    put_quad(&mut bytes, 8, 0x8811_0000);
    put_quad(&mut bytes, 16, 128);
    put_quad(&mut bytes, 24, 128);
    put_quad(&mut bytes, 32, runtime_size);
    bytes
}

fn scarlet() -> Vec<u8> {
    let mut bytes = vec![0; 4096];
    put_quad(&mut bytes, 8, 0x20_0000);
    put_quad(&mut bytes, 16, 0x110_0000);
    bytes[56..60].copy_from_slice(b"ARM\x64");
    bytes
}

#[test]
fn raw_image_reports_runtime_including_bss_instead_of_file_size() {
    let bytes = scarlet();
    let image = ScarletImage::parse(&bytes).unwrap();
    assert_eq!(image.runtime.start(), 0x8020_0000);
    assert_eq!(image.runtime.end(), 0x8130_0000);
    assert!(image.image_size > bytes.len() as u64);
}

#[test]
fn raw_image_rejects_wrong_abi_zero_size_and_dtb_overlap() {
    let mut bytes = scarlet();
    put_quad(&mut bytes, 8, 0x80_0000);
    assert_eq!(ScarletImage::parse(&bytes).unwrap_err(), ImageError::Header);
    put_quad(&mut bytes, 8, 0x20_0000);
    put_quad(&mut bytes, 16, 0);
    assert_eq!(
        ScarletImage::parse(&bytes).unwrap_err(),
        ImageError::RuntimeExtent
    );
    put_quad(&mut bytes, 16, u64::MAX);
    assert_eq!(
        ScarletImage::parse(&bytes).unwrap_err(),
        ImageError::RuntimeExtent
    );
    put_quad(&mut bytes, 16, 0xd00_0001);
    assert_eq!(
        ScarletImage::parse(&bytes).unwrap_err(),
        ImageError::RuntimeExtent
    );
    put_quad(&mut bytes, 16, 0x110_0000);
    put_quad(&mut bytes, 24, 1);
    assert_eq!(ScarletImage::parse(&bytes).unwrap_err(), ImageError::Header);
}

#[test]
fn uboot_runtime_covers_control_fdt_and_bss() {
    let bytes = uboot(0);
    let image = UbootImage::parse(&bytes).unwrap();
    assert_eq!(image.end_offset, 128);
    assert_eq!(image.runtime.size(), bytes.len() as u64 + 4096);
    image.validate_hekate_patches(&bytes).unwrap();
}

#[test]
fn all_hekate_uart_modes_preserve_unrelated_bytes_and_fdt_structure() {
    let original = uboot(0);
    let image = UbootImage::parse(&original).unwrap();
    for port in 0..=3 {
        let mut patched = original.clone();
        image.apply_hekate_uart_patch(&mut patched, port).unwrap();
        let fdt = Fdt::parse(&patched[128..]).unwrap();
        if port == 0 {
            assert_eq!(patched, original);
            continue;
        }
        let node = format!(
            "/serial@{}",
            ["70006000", "70006040", "70006200"][usize::from(port - 1)]
        );
        for name in ["stdout-path", "stderr-path"] {
            let p = fdt.find_property("/chosen", name).unwrap().unwrap();
            assert_eq!(p.data.split(|b| *b == 0).next(), Some(node.as_bytes()));
        }
        let p = fdt.find_property(&node, "status").unwrap().unwrap();
        assert_eq!(p.data.split(|b| *b == 0).next(), Some(b"okay".as_slice()));
        let status = 128 + 0x1c94 + usize::from(port - 1) * 0x12c;
        for (i, (a, b)) in original.iter().zip(&patched).enumerate() {
            if a != b {
                assert!(
                    [
                        (status, status + 5),
                        (128 + 0x3f34, 128 + 0x3f3d),
                        (128 + 0x3f54, 128 + 0x3f5d)
                    ]
                    .iter()
                    .any(|&(start, end)| start <= i && i < end)
                );
            }
        }
    }
}

#[test]
fn incompatible_hekate_layout_and_bad_ports_fail_before_any_write() {
    let mut shifted = uboot(4);
    let original = shifted.clone();
    let image = UbootImage::parse(&shifted).unwrap();
    assert_eq!(
        image.apply_hekate_uart_patch(&mut shifted, 1),
        Err(ImageError::HekatePatchLayout)
    );
    assert_eq!(shifted, original);
    let mut bytes = uboot(0);
    let original = bytes.clone();
    let image = UbootImage::parse(&bytes).unwrap();
    assert_eq!(
        image.apply_hekate_uart_patch(&mut bytes, 4),
        Err(ImageError::InvalidUartPort)
    );
    assert_eq!(bytes, original);
}

#[test]
fn every_truncated_prefix_is_rejected_without_panicking() {
    let bytes = uboot(0);
    for length in 0..bytes.len() {
        assert!(
            UbootImage::parse(&bytes[..length]).is_err(),
            "length={length}"
        );
    }
}

#[test]
fn corrupt_fdt_blocks_tokens_strings_and_reservations_are_rejected() {
    let original = fdt(0);
    let mut bytes = original.clone();
    put_word(&mut bytes, 12, 56);
    assert!(matches!(Fdt::parse(&bytes), Err(FdtError::BlockOverlap)));
    let mut bytes = original.clone();
    put_word(&mut bytes, 56, 0xff);
    assert!(Fdt::parse(&bytes).is_err());
    let mut bytes = original.clone();
    put_word(&mut bytes, 0x1c94 - 4, u32::MAX);
    assert!(matches!(Fdt::parse(&bytes), Err(FdtError::String)));
    let mut bytes = original.clone();
    let size = bytes.len();
    put_word(&mut bytes, 16, (size & !7) as u32);
    assert!(matches!(
        Fdt::parse(&bytes),
        Err(FdtError::ReservationTable)
    ));
}

#[test]
fn header_offsets_and_runtime_overflow_are_rejected() {
    let mut bytes = uboot(0);
    put_quad(&mut bytes, 16, 1 << 32);
    assert_eq!(UbootImage::parse(&bytes).unwrap_err(), ImageError::Header);
    put_quad(&mut bytes, 16, 128);
    put_quad(&mut bytes, 32, u64::MAX);
    assert_eq!(
        UbootImage::parse(&bytes).unwrap_err(),
        ImageError::RuntimeExtent
    );
}
