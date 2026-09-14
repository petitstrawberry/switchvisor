//! Decode the supported AArch64 Stage-2 MMIO aborts without reading guest instructions.
use crate::{IPA_LIMIT, mc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub offset: u64,
    pub register: usize,
    pub write: bool,
    pub sign_extend: bool,
}

impl Access {
    pub fn decode(esr: u64, far: u64, hpfar: u64) -> Option<Self> {
        // Lower-EL AArch64 data abort, 32-bit instruction, valid ISS and FAR,
        // ordinary access (not a Stage-1 table walk/cache maintenance/external abort).
        if esr >> 26 != 0x24
            || esr & (1 << 25) == 0
            || esr & (1 << 24) == 0
            || esr & (3 << 22) != (2 << 22)
            || esr & 0x780 != 0
            || esr & 0x3f != 7
        // Level-3 translation fault
        {
            return None;
        }
        let mask = ((IPA_LIMIT - 1) >> 8) & !0xf;
        if hpfar & !mask != 0 {
            return None;
        }
        let ipa = ((hpfar & mask) << 8) | (far & 0xfff);
        let offset = ipa.checked_sub(mc::BASE)?;
        if offset >= mc::SIZE || offset & 3 != 0 {
            return None;
        }
        let sign_extend = esr & (1 << 21) != 0;
        let wide = esr & (1 << 15) != 0;
        let write = esr & (1 << 6) != 0;
        if sign_extend && (!wide || write) || write && wide {
            return None;
        }
        Some(Self {
            offset,
            register: ((esr >> 16) & 31) as usize,
            write,
            sign_extend,
        })
    }

    pub fn store_value(self, registers: &[u64; 31]) -> u32 {
        registers.get(self.register).copied().unwrap_or(0) as u32
    }

    pub fn load_value(self, value: u32, registers: &mut [u64; 31]) {
        if let Some(destination) = registers.get_mut(self.register) {
            *destination = if self.sign_extend {
                i64::from(value as i32) as u64
            } else {
                u64::from(value)
            };
        }
    }
}
