use switchvisor_core::{
    fdt::{Fdt, FdtError},
    fdt_memory,
    memory::AddressRange,
    payload::RESIDENT_SIZE,
};

fn put(bytes: &mut [u8], offset: usize, value: usize) {
    bytes[offset..offset + 4].copy_from_slice(&(value as u32).to_be_bytes());
}
fn word(bytes: &[u8], offset: usize) -> usize {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}
fn begin(bytes: &mut Vec<u8>, name: &str) {
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
}
fn prop(bytes: &mut Vec<u8>, strings: &mut Vec<u8>, name: &str, data: &[u8]) {
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&(strings.len() as u32).to_be_bytes());
    strings.extend_from_slice(name.as_bytes());
    strings.push(0);
    bytes.extend_from_slice(data);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
}
fn number(bytes: &mut Vec<u8>, value: u64, cells: usize) {
    if cells == 1 {
        bytes.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
}

fn fixture(
    address_cells: usize,
    size_cells: usize,
    banks: &[(&str, u64, u64)],
    chosen: bool,
) -> Vec<u8> {
    let mut bytes = vec![0; 48];
    bytes.extend_from_slice(&0xf5a00000u64.to_be_bytes());
    bytes.extend_from_slice(&0x400000u64.to_be_bytes());
    bytes.extend_from_slice(&[0; 16]);
    let structure = bytes.len();
    let mut strings = Vec::new();
    begin(&mut bytes, "");
    prop(
        &mut bytes,
        &mut strings,
        "#address-cells",
        &(address_cells as u32).to_be_bytes(),
    );
    prop(
        &mut bytes,
        &mut strings,
        "#size-cells",
        &(size_cells as u32).to_be_bytes(),
    );
    prop(&mut bytes, &mut strings, "compatible", b"test,switch\0");
    for &(name, start, size) in banks {
        begin(&mut bytes, name);
        prop(&mut bytes, &mut strings, "device_type", b"memory\0");
        let mut data = Vec::new();
        number(&mut data, start, address_cells);
        number(&mut data, size, size_cells);
        prop(&mut bytes, &mut strings, "reg", &data);
        bytes.extend_from_slice(&2u32.to_be_bytes());
    }
    if chosen {
        begin(&mut bytes, "chosen");
        prop(&mut bytes, &mut strings, "bootargs", b"maxcpus=1\0");
        prop(
            &mut bytes,
            &mut strings,
            "linux,initrd-start",
            &0x92000000u64.to_be_bytes(),
        );
        bytes.extend_from_slice(&2u32.to_be_bytes());
    }
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&9u32.to_be_bytes());
    let string_start = bytes.len();
    bytes.extend_from_slice(&strings);
    let size = bytes.len();
    for (offset, value) in [
        (0, 0xd00dfeed),
        (4, size),
        (8, structure),
        (12, string_start),
        (16, 48),
        (20, 17),
        (24, 16),
        (32, strings.len()),
        (36, string_start - structure),
    ] {
        put(&mut bytes, offset, value);
    }
    bytes
}

fn exclude(input: &[u8]) -> Vec<u8> {
    let mut output = vec![0; input.len() + 4096];
    let size = fdt_memory::exclude(
        input,
        &mut output,
        AddressRange::new(RESIDENT_BASE, RESIDENT_SIZE).unwrap(),
    )
    .unwrap();
    output.truncate(size);
    output
}

#[test]
fn splits_memory_preserves_high_ram_and_firmware_handoff_and_is_idempotent() {
    let input = fixture(
        2,
        2,
        &[
            ("memory@80000000", 0x80000000, 0x78000000),
            ("memory@100000000", 0x100000000, 0x80000000),
        ],
        true,
    );
    let before = input.clone();
    let output = exclude(&input);
    let fdt = Fdt::parse(&output).unwrap();
    let expected = [0x80000000u64, 0x30000000, 0xb1000000, 0x47000000]
        .iter()
        .flat_map(|n| n.to_be_bytes())
        .collect::<Vec<_>>();
    assert_eq!(
        fdt.find_property("/memory@80000000", "reg")
            .unwrap()
            .unwrap()
            .data,
        expected
    );
    assert_eq!(
        fdt.find_property("/memory@100000000", "reg")
            .unwrap()
            .unwrap()
            .data,
        [0x100000000u64, 0x80000000]
            .iter()
            .flat_map(|n| n.to_be_bytes())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        fdt.find_property("/chosen", "bootargs")
            .unwrap()
            .unwrap()
            .data,
        b"maxcpus=1\0"
    );
    assert_eq!(
        fdt.find_property("/chosen", "linux,initrd-start")
            .unwrap()
            .unwrap()
            .data,
        0x92000000u64.to_be_bytes()
    );
    assert_eq!(
        fdt.find_property("/", "compatible").unwrap().unwrap().data,
        b"test,switch\0"
    );
    let reserve = word(&output, 16);
    assert_eq!(&output[reserve..reserve + 16], &input[48..64]);
    assert_eq!(
        &output[reserve + 16..reserve + 32],
        [RESIDENT_BASE, RESIDENT_SIZE]
            .iter()
            .flat_map(|n| n.to_be_bytes())
            .collect::<Vec<_>>()
    );
    assert_eq!(input, before);
    assert_eq!(exclude(&output), output);
}

#[test]
fn handles_one_cell_and_mixed_cell_geometry_and_creates_chosen() {
    for (address, size) in [(1, 1), (1, 2), (2, 1), (2, 2)] {
        let input = fixture(address, size, &[("memory", 0x80000000, 0x40000000)], false);
        let output = exclude(&input);
        let fdt = Fdt::parse(&output).unwrap();
        let mut expected = Vec::new();
        for (start, len) in [(0x80000000, 0x30000000), (0xb1000000, 0xf000000)] {
            number(&mut expected, start, address);
            number(&mut expected, len, size);
        }
        assert_eq!(
            fdt.find_property("/memory", "reg").unwrap().unwrap().data,
            expected
        );
        assert_eq!(
            fdt.find_property("/chosen", fdt_memory::RESIDENT_PROPERTY)
                .unwrap()
                .unwrap()
                .data,
            [RESIDENT_BASE, RESIDENT_SIZE]
                .iter()
                .flat_map(|n| n.to_be_bytes())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn accepts_valid_structure_padding_after_the_end_token() {
    let input = fixture(2, 2, &[("memory", 0x80000000, 0x40000000)], false);
    let mut padded = input.clone();
    let strings = word(&padded, 12);
    let structure_size = word(&padded, 36);
    padded.splice(strings..strings, [0; 16]);
    let total = padded.len();
    put(&mut padded, 4, total);
    put(&mut padded, 12, strings + 16);
    put(&mut padded, 36, structure_size + 16);
    Fdt::parse(&padded).unwrap();
    assert_eq!(exclude(&padded), exclude(&input));
}

#[test]
fn removes_a_whole_bank_and_handles_the_two_edges() {
    for (start, size, expected) in [
        (RESIDENT_BASE, RESIDENT_SIZE, vec![]),
        (
            RESIDENT_BASE,
            RESIDENT_SIZE + 0x200000,
            vec![RESIDENT_BASE + RESIDENT_SIZE, 0x200000],
        ),
        (
            RESIDENT_BASE - 0x200000,
            RESIDENT_SIZE + 0x200000,
            vec![RESIDENT_BASE - 0x200000, 0x200000],
        ),
    ] {
        let output = exclude(&fixture(2, 2, &[("memory", start, size)], false));
        let fdt = Fdt::parse(&output).unwrap();
        assert_eq!(
            fdt.find_property("/memory", "reg").unwrap().unwrap().data,
            expected
                .iter()
                .flat_map(|n| n.to_be_bytes())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn rejects_conflicting_reservations_invalid_cells_overflows_and_small_output() {
    let region = AddressRange::new(RESIDENT_BASE, RESIDENT_SIZE).unwrap();
    let input = fixture(2, 2, &[("memory", 0x80000000, 0x78000000)], false);
    for end in 0..40 {
        assert!(fdt_memory::exclude(&input[..end], &mut [0; 4096], region).is_err());
    }
    assert_eq!(
        fdt_memory::exclude(&input, &mut [0; 40], region),
        Err(FdtError::OutputCapacity)
    );
    let mut conflict = input.clone();
    conflict[48..56].copy_from_slice(&RESIDENT_BASE.to_be_bytes());
    assert_eq!(
        fdt_memory::exclude(&conflict, &mut [0; 4096], region),
        Err(FdtError::ReservationConflict)
    );
    let invalid = fixture(3, 2, &[], false);
    assert_eq!(
        fdt_memory::exclude(&invalid, &mut [0; 4096], region),
        Err(FdtError::MemoryCells)
    );
    let overflow = fixture(2, 2, &[("memory", u64::MAX - 10, 100)], false);
    assert_eq!(
        fdt_memory::exclude(&overflow, &mut [0; 4096], region),
        Err(FdtError::MemoryRanges)
    );
}

// Synthetic interior hole; independent of the platform resident placement.
const RESIDENT_BASE: u64 = 0xb000_0000;
