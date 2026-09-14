use switchvisor_core::{mc, mmio::Access};

fn esr(register: u64, write: bool) -> u64 {
    (0x24 << 26)
        | (1 << 25)
        | (1 << 24)
        | (2 << 22)
        | (register << 16)
        | (u64::from(write) << 6)
        | 7
}

fn decode(esr: u64, address: u64) -> Option<Access> {
    Access::decode(esr, address, (address >> 8) & !15)
}

#[test]
fn hpfar_selects_the_ipa_even_when_guest_stage1_uses_an_alias() {
    let address = mc::BASE + mc::GSC5_BOM;
    let access = Access::decode(esr(17, false), 0x60010d4c, (address >> 8) & !15).unwrap();
    assert_eq!(access.offset, mc::GSC5_BOM);
    assert_eq!(access.register, 17);
    let mut registers = [u64::MAX; 31];
    access.load_value(0x80000000, &mut registers);
    assert_eq!(registers[17], 0x80000000);
    assert!(registers[..17].iter().all(|&r| r == u64::MAX));
    assert!(registers[18..].iter().all(|&r| r == u64::MAX));
    let signed = decode(esr(30, false) | (1 << 21) | (1 << 15), address).unwrap();
    signed.load_value(0x80000000, &mut registers);
    assert_eq!(registers[30], 0xffffffff80000000);
}

#[test]
fn writes_take_only_low_word_and_zero_register_is_preserved() {
    let registers = [0xfeedface12345678; 31];
    assert_eq!(
        decode(esr(30, true), mc::BASE)
            .unwrap()
            .store_value(&registers),
        0x12345678
    );
    let store_zero = decode(esr(31, true), mc::BASE).unwrap();
    assert_eq!(store_zero.store_value(&registers), 0);
    let mut output = registers;
    decode(esr(31, false), mc::BASE)
        .unwrap()
        .load_value(1, &mut output);
    assert_eq!(registers, output);
}

#[test]
fn unsupported_abort_syndromes_and_outside_addresses_are_rejected() {
    let valid = esr(0, false);
    for syndrome in [
        valid ^ (1 << 25),
        valid ^ (1 << 24),
        valid ^ (1 << 26),
        valid ^ (1 << 22),
        valid | (1 << 22),
        valid | (1 << 10),
        valid | (1 << 9),
        valid | (1 << 8),
        valid | (1 << 7),
        valid ^ 1,
        valid | (1 << 21),
        esr(0, true) | (1 << 15),
        esr(0, true) | (1 << 21) | (1 << 15),
    ] {
        assert_eq!(decode(syndrome, mc::BASE), None, "{syndrome:x}");
    }
    for address in [mc::BASE - 4, mc::BASE + 1, mc::BASE + mc::SIZE, 1 << 36] {
        assert_eq!(decode(valid, address), None);
    }
    assert!(decode(valid | (1 << 14), mc::BASE + mc::SIZE - 4).is_some());
}
