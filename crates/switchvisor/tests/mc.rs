use switchvisor::{
    mc::{self, PlacementError},
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
};

fn registers() -> [u32; 1024] {
    let mut registers = [0; 1024];
    registers[0x50 / 4] = 4096;
    for (offset, address, size) in [
        (0xc0c, 0xffe00000, 8),
        (0xc5c, 0xffd00000, 2),
        (0xcac, 0xffd40000, 6),
    ] {
        registers[offset / 4] = address;
        registers[(offset + 8) / 4] = size;
    }
    registers[0x670 / 4] = 0xfff00000;
    registers[0x674 / 4] = 1;
    registers
}

#[test]
fn standard_and_swiss_cheese_ram_discovery_leave_room_for_resident_el2() {
    let mut words = registers();
    assert_eq!(
        mc::ram_end(|offset| words[offset as usize / 4]),
        Ok(0x1_8000_0000)
    );
    assert_eq!(mc::validate(|offset| words[offset as usize / 4]), Ok(()));
    words[0x50 / 4] = 0x80001800;
    assert_eq!(
        mc::ram_end(|offset| words[offset as usize / 4]),
        Ok(0x1_8000_0000)
    );
    assert_eq!(mc::validate(|offset| words[offset as usize / 4]), Ok(()));
    words[0x50 / 4] = 0x800007ff;
    assert_eq!(
        mc::validate(|offset| words[offset as usize / 4]),
        Err(PlacementError::RamSize)
    );
    words[0x50 / 4] = !(1 << 31);
    assert_eq!(
        mc::validate(|offset| words[offset as usize / 4]),
        Err(PlacementError::RamSize)
    );
    words[0x50 / 4] = 1024;
    assert!(mc::validate(|offset| words[offset as usize / 4]).is_err());
}

#[test]
fn every_low_firmware_carveout_limits_usable_ram_before_copying() {
    for (bom, hi, size) in [
        (0x670, 0x9d4, 0x674),
        (0x9a0, 0x9a8, 0x9a4),
        (0x648, 0x978, 0x64c),
        (0xc0c, 0xc10, 0xc14),
        (0xc5c, 0xc60, 0xc64),
        (0xcac, 0xcb0, 0xcb4),
        (0xcfc, 0xd00, 0xd04),
    ] {
        let mut words = registers();
        words[bom / 4] = (RESIDENT_BASE + RESIDENT_SIZE - 1) as u32;
        words[size / 4] = 1;
        assert_eq!(
            mc::validate(|offset| words[offset as usize / 4]),
            Err(PlacementError::FirmwareCarveout)
        );
        words[bom / 4] = (RESIDENT_BASE + RESIDENT_SIZE) as u32;
        assert_eq!(mc::validate(|offset| words[offset as usize / 4]), Ok(()));
        words[bom / 4] = 0x7f000000;
        words[hi / 4] = 1;
        assert_eq!(mc::validate(|offset| words[offset as usize / 4]), Ok(()));
    }
}

#[test]
fn an_existing_gsc5_is_never_repurposed() {
    let mut words = registers();
    words[mc::GSC5_BOM as usize / 4] = 0x7f000000;
    words[mc::GSC5_HI as usize / 4] = 1;
    words[mc::GSC5_SIZE as usize / 4] = 1;
    assert_eq!(
        mc::validate(|offset| words[offset as usize / 4]),
        Err(PlacementError::Gsc5InUse)
    );
}

#[test]
fn only_the_disabled_gsc5_descriptor_is_overridden() {
    for offset in (0..mc::SIZE).step_by(4) {
        let expected = match offset {
            mc::GSC5_BOM => Some(RESIDENT_BASE as u32),
            mc::GSC5_HI => Some(0),
            mc::GSC5_SIZE => Some((RESIDENT_SIZE >> 17) as u32),
            _ => None,
        };
        assert_eq!(mc::read_override(offset), expected);
    }
}
