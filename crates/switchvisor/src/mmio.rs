//! Decode the supported AArch64 Stage-2 MMIO aborts.
use crate::{IPA_LIMIT, mc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub offset: u64,
    pub register: usize,
    pub write: bool,
    pub sign_extend: bool,
    pub size: u8,
    pub wide: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Writeback {
    pub register: usize,
    pub value: u64,
}

impl Access {
    pub fn decode(esr: u64, far: u64, hpfar: u64) -> Option<Self> {
        let access = Self::decode_region(esr, far, hpfar, mc::BASE, mc::SIZE)?;
        (access.size == 4 && esr & 0x3f == 7 && (!access.sign_extend || access.wide))
            .then_some(access)
    }

    pub fn decode_region(esr: u64, far: u64, hpfar: u64, base: u64, length: u64) -> Option<Self> {
        // Lower-EL AArch64 data abort, 32-bit instruction, valid ISS and FAR,
        // ordinary access (not a Stage-1 table walk/cache maintenance/external abort).
        if esr >> 26 != 0x24
            || esr & (1 << 25) == 0
            || esr & (1 << 24) == 0
            || esr & 0x780 != 0
            || !matches!(esr & 0x3f, 6 | 7)
        // Level-2/3 translation fault, including a wholly excluded USB block.
        {
            return None;
        }
        let ipa = ipa(far, hpfar)?;
        let size = 1u8 << ((esr >> 22) & 3);
        let offset = ipa.checked_sub(base)?;
        if offset.checked_add(u64::from(size))? > length || offset % u64::from(size) != 0 {
            return None;
        }
        let sign_extend = esr & (1 << 21) != 0;
        let wide = esr & (1 << 15) != 0;
        let write = esr & (1 << 6) != 0;
        if sign_extend && (write || size == 8)
            || size == 8 && !wide
            || size != 8 && !sign_extend && wide
        {
            return None;
        }
        Some(Self {
            offset,
            register: ((esr >> 16) & 31) as usize,
            write,
            sign_extend,
            size,
            wide,
        })
    }

    /// Decode an integer STR with post-index writeback when hardware omits the
    /// instruction syndrome. Arm permits ISV=0 for this addressing form, as
    /// observed from U-Boot's GIC distributor initialization on Cortex-A57.
    pub fn decode_store_post_index_region(
        esr: u64,
        far: u64,
        hpfar: u64,
        instruction: u32,
        base: u64,
        length: u64,
        registers: &[u64; 31],
    ) -> Option<(Self, Writeback)> {
        // Lower-EL AArch64 data abort, valid FAR, write translation fault at
        // level 2/3, with no architected instruction syndrome.
        if esr >> 26 != 0x24
            || esr & (1 << 25) == 0
            || esr & (1 << 24) != 0
            || esr & 0x780 != 0
            || esr & (1 << 6) == 0
            || !matches!(esr & 0x3f, 6 | 7)
            // STR{B,H,W,X} Rt, [Xn], #simm9. SIMD/FP and loads are excluded.
            || instruction & 0x3fe0_0c00 != 0x3800_0400
        {
            return None;
        }
        let source = (instruction & 31) as usize;
        let address_register = ((instruction >> 5) & 31) as usize;
        // SP writeback needs separate architectural state. Overlap is
        // constrained-unpredictable and is rejected rather than guessed.
        if address_register == 31 || address_register == source {
            return None;
        }
        let virtual_address = registers[address_register];
        if virtual_address != far {
            return None;
        }
        let size = 1u8 << (instruction >> 30);
        let physical_address = ipa(far, hpfar)?;
        let offset = physical_address.checked_sub(base)?;
        if offset.checked_add(u64::from(size))? > length || offset % u64::from(size) != 0 {
            return None;
        }
        let immediate = ((instruction >> 12) & 0x1ff) as i16;
        let immediate = (immediate << 7) >> 7;
        Some((
            Self {
                offset,
                register: source,
                write: true,
                sign_extend: false,
                size,
                wide: size == 8,
            },
            Writeback {
                register: address_register,
                value: virtual_address.wrapping_add_signed(i64::from(immediate)),
            },
        ))
    }

    pub fn store_value(self, registers: &[u64; 31]) -> u32 {
        self.store_data(registers) as u32
    }

    pub fn store_data(self, registers: &[u64; 31]) -> u64 {
        registers.get(self.register).copied().unwrap_or(0)
            & (u64::MAX >> (64 - u32::from(self.size) * 8))
    }

    pub fn load_value(self, value: u32, registers: &mut [u64; 31]) {
        self.load_data(u64::from(value), registers);
    }

    pub fn load_data(self, value: u64, registers: &mut [u64; 31]) {
        if let Some(destination) = registers.get_mut(self.register) {
            let bits = u32::from(self.size) * 8;
            let mask = u64::MAX >> (64 - bits);
            let value = value & mask;
            let signed = if self.sign_extend {
                ((value << (64 - bits)) as i64 >> (64 - bits)) as u64
            } else {
                value
            };
            *destination = if self.wide {
                signed
            } else {
                u64::from(signed as u32)
            };
        }
    }
}

fn ipa(far: u64, hpfar: u64) -> Option<u64> {
    let mask = ((IPA_LIMIT - 1) >> 8) & !0xf;
    (hpfar & !mask == 0).then_some(((hpfar & mask) << 8) | (far & 0xfff))
}
