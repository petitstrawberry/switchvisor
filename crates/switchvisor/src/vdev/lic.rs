//! Tegra210 LIC pass-through with one EL2-owned interrupt source.

use super::{DeviceError, MmioRegion, VirtualDevice};
use crate::drivers::Mmio;

pub const BASE: u64 = 0x6000_4000;
pub const SIZE: u64 = 0x1000;
const BANK_STRIDE: u64 = 0x100;
const SOURCE_COUNT: u32 = 6 * 32;
const CPU_IEP_VFIQ: u64 = 0x08;
const CPU_ISR: u64 = 0x10;
const CPU_IEP_FIR: u64 = 0x14;
const CPU_IEP_FIR_SET: u64 = 0x18;
const CPU_IEP_FIR_CLR: u64 = 0x1c;
const CPU_IER: u64 = 0x20;
const CPU_IER_SET: u64 = 0x24;
const CPU_IER_CLR: u64 = 0x28;
const CPU_IEP_CLASS: u64 = 0x2c;

pub struct Lic<M> {
    mmio: M,
    owned_source: Option<u32>,
    guest_enabled: bool,
}

impl<M: Mmio> Lic<M> {
    pub const fn new(mmio: M) -> Self {
        Self {
            mmio,
            owned_source: None,
            guest_enabled: false,
        }
    }

    pub fn set_owned_source(&mut self, source: Option<u32>) {
        self.owned_source = source.filter(|source| *source < SOURCE_COUNT);
        self.guest_enabled = false;
    }

    fn owned(&self) -> Option<(u64, u32)> {
        self.owned_source
            .map(|source| (u64::from(source / 32) * BANK_STRIDE, 1 << (source % 32)))
    }

    /// Keep the physical source routed to the CPU as an IRQ. Guest enable
    /// state is exposed separately and only controls virtual delivery.
    pub fn initialize(&mut self) {
        let Some((bank, bit)) = self.owned() else {
            return;
        };
        let class = self.mmio.read32(BASE + bank + CPU_IEP_CLASS);
        self.mmio.write32(BASE + bank + CPU_IEP_CLASS, class & !bit);
        self.mmio.write32(BASE + bank + CPU_IER_SET, bit);
        self.mmio.barrier();
    }

    pub const fn owned_source_enabled(&self) -> bool {
        self.guest_enabled
    }

    fn read_word(&mut self, offset: u64) -> u32 {
        let mut value = self.mmio.read32(BASE + offset);
        let Some((bank, bit)) = self.owned() else {
            return value;
        };
        let register = offset.checked_sub(bank);
        if matches!(
            register,
            Some(
                CPU_IEP_VFIQ
                    | CPU_ISR
                    | CPU_IEP_FIR
                    | CPU_IEP_FIR_SET
                    | CPU_IEP_FIR_CLR
                    | CPU_IER
                    | CPU_IER_SET
                    | CPU_IER_CLR
                    | CPU_IEP_CLASS
            )
        ) {
            value &= !bit;
        }
        if register == Some(CPU_IER) && self.guest_enabled {
            value |= bit;
        }
        value
    }

    fn write_word(&mut self, offset: u64, mut value: u32) {
        let Some((bank, bit)) = self.owned() else {
            self.mmio.write32(BASE + offset, value);
            self.mmio.barrier();
            return;
        };
        match offset.checked_sub(bank) {
            Some(CPU_IER_SET) => {
                self.guest_enabled |= value & bit != 0;
                value &= !bit;
            }
            Some(CPU_IER_CLR) => {
                self.guest_enabled &= value & bit == 0;
                value &= !bit;
            }
            Some(CPU_IEP_CLASS) => {
                let current = self.mmio.read32(BASE + offset);
                value = (value & !bit) | (current & bit);
            }
            Some(
                CPU_IEP_VFIQ | CPU_ISR | CPU_IEP_FIR | CPU_IEP_FIR_SET | CPU_IEP_FIR_CLR | CPU_IER,
            ) => value &= !bit,
            _ => {}
        }
        if value != 0 || !matches!(offset.checked_sub(bank), Some(CPU_IER_SET | CPU_IER_CLR)) {
            self.mmio.write32(BASE + offset, value);
        }
        self.mmio.barrier();
    }
}

impl<M: Mmio> VirtualDevice for Lic<M> {
    fn region(&self) -> MmioRegion {
        MmioRegion {
            base: BASE,
            size: SIZE,
        }
    }

    fn read(&mut self, offset: u64, size: u8) -> Result<u64, DeviceError> {
        if size != 4 || offset & 3 != 0 || offset >= SIZE {
            return Err(DeviceError::AccessSize);
        }
        Ok(u64::from(self.read_word(offset)))
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) -> Result<(), DeviceError> {
        if size != 4 || offset & 3 != 0 || offset >= SIZE {
            return Err(DeviceError::AccessSize);
        }
        self.write_word(offset, value as u32);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: u32 = 44;
    const BANK: u64 = BANK_STRIDE;
    const BIT: u32 = 1 << 12;

    struct Mock {
        words: [u32; SIZE as usize / 4],
    }

    impl Mmio for Mock {
        fn read32(&mut self, address: u64) -> u32 {
            self.words[(address - BASE) as usize / 4]
        }

        fn write32(&mut self, address: u64, value: u32) {
            let offset = address - BASE;
            let index = offset as usize / 4;
            match offset % BANK_STRIDE {
                CPU_IER_SET => self.words[(offset - 4) as usize / 4] |= value,
                CPU_IER_CLR => self.words[(offset - 8) as usize / 4] &= !value,
                CPU_IEP_FIR_SET => self.words[(offset - 4) as usize / 4] |= value,
                CPU_IEP_FIR_CLR => self.words[(offset - 8) as usize / 4] &= !value,
                _ => self.words[index] = value,
            }
        }

        fn barrier(&mut self) {}
    }

    fn lic() -> Lic<Mock> {
        let mut lic = Lic::new(Mock {
            words: [0; SIZE as usize / 4],
        });
        lic.set_owned_source(Some(SOURCE));
        lic
    }

    #[test]
    fn initialization_enables_the_physical_irq_but_hides_it_from_the_guest() {
        let mut lic = lic();
        lic.mmio.words[(BANK + CPU_IEP_CLASS) as usize / 4] = u32::MAX;
        lic.initialize();
        assert_eq!(lic.mmio.words[(BANK + CPU_IER) as usize / 4] & BIT, BIT);
        assert_eq!(lic.mmio.words[(BANK + CPU_IEP_CLASS) as usize / 4] & BIT, 0);
        assert_eq!(lic.read(BANK + CPU_IER, 4), Ok(0));
    }

    #[test]
    fn guest_enable_state_is_virtual_and_clear_all_keeps_the_physical_source() {
        let mut lic = lic();
        lic.initialize();
        lic.write(BANK + CPU_IER_SET, 4, u64::from(BIT | 2))
            .unwrap();
        assert!(lic.owned_source_enabled());
        assert_eq!(lic.read(BANK + CPU_IER, 4), Ok(u64::from(BIT | 2)));

        lic.write(BANK + CPU_IER_CLR, 4, u64::from(u32::MAX))
            .unwrap();
        assert!(!lic.owned_source_enabled());
        assert_eq!(lic.mmio.words[(BANK + CPU_IER) as usize / 4] & BIT, BIT);
        assert_eq!(lic.mmio.words[(BANK + CPU_IER) as usize / 4] & 2, 0);
    }

    #[test]
    fn guest_cannot_reclassify_or_force_the_owned_source() {
        let mut lic = lic();
        lic.initialize();
        lic.write(BANK + CPU_IEP_CLASS, 4, u64::from(u32::MAX))
            .unwrap();
        lic.write(BANK + CPU_IEP_FIR_SET, 4, u64::from(BIT | 4))
            .unwrap();
        assert_eq!(lic.mmio.words[(BANK + CPU_IEP_CLASS) as usize / 4] & BIT, 0);
        assert_eq!(lic.mmio.words[(BANK + CPU_IEP_FIR) as usize / 4] & BIT, 0);
        assert_eq!(lic.mmio.words[(BANK + CPU_IEP_FIR) as usize / 4] & 4, 4);
    }
}
