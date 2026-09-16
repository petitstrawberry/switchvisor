//! Guest GIC distributor pass-through with EL2-owned interrupt protection.

use crate::{
    drivers::{
        Mmio,
        interrupt::gicv2::{Layout, MAINTENANCE_IRQ, SPURIOUS_IRQ},
    },
    vdev::{DeviceError, MmioRegion, VirtualDevice},
};

const GICD_CTLR: u64 = 0x000;
const GICD_IGROUPR0: u64 = 0x080;
const GICD_ISENABLER0: u64 = 0x100;
const GICD_ICENABLER0: u64 = 0x180;
const GICD_ISPENDR0: u64 = 0x200;
const GICD_ICPENDR0: u64 = 0x280;
const GICD_ISACTIVER0: u64 = 0x300;
const GICD_ICACTIVER0: u64 = 0x380;
const GICD_IPRIORITYR0: u64 = 0x400;
const GICD_IPRIORITYR3: u64 = 0x40c;
const GICD_IPRIORITYR6: u64 = 0x418;
const GICD_ITARGETSR0: u64 = 0x800;
const GICD_ICFGR0: u64 = 0xc00;
const GICD_ICFGR1: u64 = 0xc04;
const GICD_SGIR: u64 = 0xf00;
const GICD_CPENDSGIR3: u64 = 0xf1c;
const GICD_SPENDSGIR3: u64 = 0xf2c;

const MAINTENANCE_BIT: u32 = 1 << MAINTENANCE_IRQ;
const MAINTENANCE_PRIORITY: u32 = 0xff << 8;
const MAINTENANCE_CONFIG: u32 = 3 << 18;
const DEBUG_SGI_BIT: u32 = 1 << 15;
const DEBUG_SGI_BYTE: u32 = 0xff << 24;

pub struct Distributor<M> {
    mmio: M,
    layout: Layout,
    guest_control: u32,
    owned_interrupt: u32,
    owned_interrupt_enabled: bool,
    debug_sgi: bool,
}

impl<M: Mmio> Distributor<M> {
    pub const fn new(mmio: M, layout: Layout) -> Self {
        Self {
            mmio,
            layout,
            guest_control: 0,
            owned_interrupt: SPURIOUS_IRQ,
            owned_interrupt_enabled: false,
            debug_sgi: false,
        }
    }

    pub fn set_owned_interrupt(&mut self, interrupt: Option<u32>) {
        self.owned_interrupt = interrupt
            .filter(|id| (32..1020).contains(id))
            .unwrap_or(SPURIOUS_IRQ);
        self.owned_interrupt_enabled = false;
    }

    pub fn set_debug_sgi(&mut self, enabled: bool) {
        self.debug_sgi = enabled;
    }

    /// Keep the physical Non-secure group enabled for the maintenance PPI,
    /// while retaining the control value observed by the guest.
    pub fn initialize(&mut self) {
        self.guest_control = self.mmio.read32(self.layout.distributor + GICD_CTLR) & 1;
        self.mmio
            .write32(self.layout.distributor + GICD_CTLR, self.guest_control | 1);
        self.mmio.barrier();
    }

    pub const fn enabled(&self) -> bool {
        self.guest_control & 1 != 0
    }

    pub const fn owned_interrupt_enabled(&self) -> bool {
        self.owned_interrupt_enabled
    }

    fn owned_word_mask(&self, offset: u64, base: u64) -> u32 {
        let id = self.owned_interrupt;
        if id < 1020 && offset == base + u64::from(id / 32) * 4 {
            1 << (id % 32)
        } else {
            0
        }
    }

    fn owned_byte_mask(&self, offset: u64, base: u64) -> u32 {
        let id = self.owned_interrupt;
        if id < 1020 && offset == base + u64::from(id / 4) * 4 {
            0xff << ((id % 4) * 8)
        } else {
            0
        }
    }

    fn owned_config_mask(&self, offset: u64) -> u32 {
        let id = self.owned_interrupt;
        if id < 1020 && offset == GICD_ICFGR0 + u64::from(id / 16) * 4 {
            3 << ((id % 16) * 2)
        } else {
            0
        }
    }

    fn owned_action_mask(&self, offset: u64) -> u32 {
        for base in [
            GICD_IGROUPR0,
            GICD_ISENABLER0,
            GICD_ICENABLER0,
            GICD_ISPENDR0,
            GICD_ICPENDR0,
            GICD_ISACTIVER0,
            GICD_ICACTIVER0,
        ] {
            let mask = self.owned_word_mask(offset, base);
            if mask != 0 {
                return mask;
            }
        }
        0
    }

    fn protected_mask(&self, offset: u64) -> u32 {
        let maintenance = match offset {
            GICD_IGROUPR0 | GICD_ISENABLER0 | GICD_ICENABLER0 | GICD_ISPENDR0 | GICD_ICPENDR0
            | GICD_ISACTIVER0 | GICD_ICACTIVER0 => MAINTENANCE_BIT,
            GICD_IPRIORITYR6 => MAINTENANCE_PRIORITY,
            GICD_ICFGR1 => MAINTENANCE_CONFIG,
            _ => 0,
        };
        let debug = if self.debug_sgi {
            match offset {
                GICD_IGROUPR0 | GICD_ISENABLER0 | GICD_ICENABLER0 | GICD_ISPENDR0
                | GICD_ICPENDR0 | GICD_ISACTIVER0 | GICD_ICACTIVER0 => DEBUG_SGI_BIT,
                GICD_IPRIORITYR3 | GICD_CPENDSGIR3 | GICD_SPENDSGIR3 => DEBUG_SGI_BYTE,
                _ => 0,
            }
        } else {
            0
        };
        maintenance
            | debug
            | self.owned_action_mask(offset)
            | self.owned_byte_mask(offset, GICD_IPRIORITYR0)
            | self.owned_byte_mask(offset, GICD_ITARGETSR0)
            | self.owned_config_mask(offset)
    }

    fn is_write_one(offset: u64) -> bool {
        matches!(offset, 0x100..0x400 | 0xf10..0xf30)
    }

    fn decode_access(offset: u64, size: u8) -> Result<(u64, u32, u32), DeviceError> {
        if !matches!(size, 1 | 2 | 4) {
            return Err(DeviceError::AccessSize);
        }
        let word = offset & !3;
        if offset + u64::from(size) > word + 4 {
            return Err(DeviceError::AccessSize);
        }
        let shift = ((offset & 3) * 8) as u32;
        let mask = (u32::MAX >> (32 - u32::from(size) * 8)) << shift;
        Ok((word, shift, mask))
    }

    fn guest_word(&mut self, offset: u64) -> u32 {
        if offset == GICD_CTLR {
            return self.guest_control;
        }
        let mut value = self.mmio.read32(self.layout.distributor + offset);
        if matches!(
            offset,
            GICD_ISENABLER0
                | GICD_ICENABLER0
                | GICD_ISPENDR0
                | GICD_ICPENDR0
                | GICD_ISACTIVER0
                | GICD_ICACTIVER0
        ) {
            value &= !MAINTENANCE_BIT;
        }
        let owned = self.owned_action_mask(offset);
        if owned != 0 {
            value &= !owned;
            if self.owned_interrupt_enabled
                && (self.owned_word_mask(offset, GICD_ISENABLER0) != 0
                    || self.owned_word_mask(offset, GICD_ICENABLER0) != 0)
            {
                value |= owned;
            }
        }
        if self.debug_sgi {
            value &= match offset {
                GICD_IGROUPR0 | GICD_ISENABLER0 | GICD_ICENABLER0 | GICD_ISPENDR0
                | GICD_ICPENDR0 | GICD_ISACTIVER0 | GICD_ICACTIVER0 => !DEBUG_SGI_BIT,
                GICD_IPRIORITYR3 | GICD_CPENDSGIR3 | GICD_SPENDSGIR3 => !DEBUG_SGI_BYTE,
                _ => u32::MAX,
            };
        }
        value
    }
}

impl<M: Mmio> VirtualDevice for Distributor<M> {
    fn region(&self) -> MmioRegion {
        MmioRegion {
            base: self.layout.distributor,
            size: self.layout.distributor_size,
        }
    }

    fn read(&mut self, offset: u64, size: u8) -> Result<u64, DeviceError> {
        let (word, shift, mask) = Self::decode_access(offset, size)?;
        Ok(u64::from((self.guest_word(word) & mask) >> shift))
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) -> Result<(), DeviceError> {
        let (word, shift, mask) = Self::decode_access(offset, size)?;
        let value = ((value as u32) << shift) & mask;
        let address = self.layout.distributor + word;

        if word == GICD_SGIR {
            // SGIR is write-only: a read/modify/write of a partial access can
            // reissue a stale SGI. Send only the guest's supplied bits.
            if mask & 0xf == 0 {
                return Err(DeviceError::AccessSize);
            }
            if !(self.debug_sgi && value & 0xf == 15) {
                self.mmio.write32(address, value);
                self.mmio.barrier();
            }
            return Ok(());
        }

        if value & self.owned_word_mask(word, GICD_ISENABLER0) != 0 {
            self.owned_interrupt_enabled = true;
        }
        if value & self.owned_word_mask(word, GICD_ICENABLER0) != 0 {
            self.owned_interrupt_enabled = false;
        }

        if word == GICD_CTLR {
            self.guest_control = ((self.guest_control & !mask) | value) & 1;
            self.mmio.write32(address, self.guest_control | 1);
        } else if Self::is_write_one(word) {
            self.mmio
                .write32(address, value & !self.protected_mask(word));
        } else {
            let current = self.mmio.read32(address);
            let merged = (current & !mask) | value;
            let protected = self.protected_mask(word);
            self.mmio
                .write32(address, (merged & !protected) | (current & protected));
        }
        self.mmio.barrier();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: Layout = Layout {
        distributor: 0,
        distributor_size: 0x1000,
        cpu: 0,
        cpu_size: 0x2000,
        hypervisor: 0,
        hypervisor_size: 0x2000,
        virtual_cpu: 0,
        virtual_cpu_size: 0x2000,
    };

    struct Mock {
        words: [u32; 1024],
    }

    impl Mmio for Mock {
        fn read32(&mut self, address: u64) -> u32 {
            self.words[address as usize / 4]
        }
        fn write32(&mut self, address: u64, value: u32) {
            self.words[address as usize / 4] = value;
        }
        fn barrier(&mut self) {}
    }

    fn distributor() -> Distributor<Mock> {
        Distributor::new(Mock { words: [0; 1024] }, LAYOUT)
    }

    #[test]
    fn guest_cannot_disable_the_physical_group() {
        let mut distributor = distributor();
        distributor.initialize();
        distributor.write(GICD_CTLR, 4, 0).unwrap();
        assert!(!distributor.enabled());
        assert_eq!(distributor.read(GICD_CTLR, 4), Ok(0));
        assert_eq!(distributor.mmio.words[0], 1);
        distributor
            .write(GICD_CTLR, 4, u64::from(u32::MAX))
            .unwrap();
        assert!(distributor.enabled());
        assert_eq!(distributor.read(GICD_CTLR, 4), Ok(1));
        assert_eq!(distributor.mmio.words[0], 1);
    }

    #[test]
    fn maintenance_ppi_is_filtered_from_action_registers() {
        let mut distributor = distributor();
        distributor
            .write(GICD_ICENABLER0, 4, u64::from(u32::MAX))
            .unwrap();
        assert_eq!(
            distributor.mmio.words[GICD_ICENABLER0 as usize / 4],
            !MAINTENANCE_BIT
        );
    }

    #[test]
    fn maintenance_priority_and_configuration_are_preserved() {
        let mut distributor = distributor();
        distributor.mmio.words[GICD_IPRIORITYR6 as usize / 4] = 0x1122_3344;
        distributor.write(GICD_IPRIORITYR6, 4, 0xaabb_ccdd).unwrap();
        assert_eq!(
            distributor.mmio.words[GICD_IPRIORITYR6 as usize / 4],
            0xaabb_33dd
        );

        distributor.mmio.words[GICD_ICFGR1 as usize / 4] = MAINTENANCE_CONFIG;
        distributor.write(GICD_ICFGR1, 4, 0).unwrap();
        assert_eq!(
            distributor.mmio.words[GICD_ICFGR1 as usize / 4] & MAINTENANCE_CONFIG,
            MAINTENANCE_CONFIG
        );
    }

    #[test]
    fn guest_enable_state_is_virtualized_for_an_el2_owned_spi() {
        const OWNED: u32 = 76;
        let mut distributor = distributor();
        distributor.set_owned_interrupt(Some(OWNED));
        distributor.initialize();
        let register = u64::from(OWNED / 32) * 4;
        let bit = 1u32 << (OWNED % 32);

        distributor
            .write(GICD_ISENABLER0 + register, 4, u64::from(bit))
            .unwrap();
        assert!(distributor.owned_interrupt_enabled());
        assert_eq!(
            distributor.mmio.words[(GICD_ISENABLER0 + register) as usize / 4],
            0
        );
        assert_eq!(
            distributor.read(GICD_ISENABLER0 + register, 4),
            Ok(u64::from(bit))
        );
        assert_eq!(
            distributor.read(GICD_ICENABLER0 + register, 4),
            Ok(u64::from(bit))
        );

        distributor
            .write(GICD_ICENABLER0 + register, 4, u64::from(bit))
            .unwrap();
        assert!(!distributor.owned_interrupt_enabled());
        assert_eq!(distributor.read(GICD_ISENABLER0 + register, 4), Ok(0));
    }

    #[test]
    fn guest_cannot_reconfigure_an_el2_owned_spi() {
        const OWNED: u32 = 76;
        let mut distributor = distributor();
        distributor.set_owned_interrupt(Some(OWNED));
        let priority = GICD_IPRIORITYR0 + u64::from(OWNED / 4) * 4;
        let target = GICD_ITARGETSR0 + u64::from(OWNED / 4) * 4;
        let config = GICD_ICFGR0 + u64::from(OWNED / 16) * 4;
        distributor.mmio.words[priority as usize / 4] = 0x4433_2211;
        distributor.mmio.words[target as usize / 4] = 0x8877_6655;
        distributor.mmio.words[config as usize / 4] = 0xaaaa_aaaa;

        distributor.write(priority, 4, u64::from(u32::MAX)).unwrap();
        distributor.write(target, 4, 0).unwrap();
        distributor.write(config, 4, 0).unwrap();

        assert_eq!(distributor.mmio.words[priority as usize / 4], 0xffff_ff11);
        assert_eq!(distributor.mmio.words[target as usize / 4], 0x0000_0055);
        assert_eq!(
            distributor.mmio.words[config as usize / 4] & (3 << 24),
            2 << 24
        );
    }

    #[test]
    fn debug_sgi_is_invisible_and_cannot_be_reconfigured_or_generated() {
        let mut distributor = distributor();
        distributor.set_debug_sgi(true);
        for register in [
            GICD_IGROUPR0,
            GICD_ISENABLER0,
            GICD_ICENABLER0,
            GICD_ISPENDR0,
            GICD_ICPENDR0,
            GICD_ISACTIVER0,
            GICD_ICACTIVER0,
        ] {
            distributor.mmio.words[register as usize / 4] = DEBUG_SGI_BIT;
            assert_eq!(
                distributor.read(register, 4).unwrap() as u32 & DEBUG_SGI_BIT,
                0
            );
            distributor
                .write(register, 4, u64::from(DEBUG_SGI_BIT))
                .unwrap();
            assert_eq!(
                distributor.mmio.words[register as usize / 4] & DEBUG_SGI_BIT,
                if register == GICD_IGROUPR0 {
                    DEBUG_SGI_BIT
                } else {
                    0
                }
            );
        }
        distributor.mmio.words[GICD_IPRIORITYR3 as usize / 4] = 0x1122_3344;
        distributor
            .write(GICD_IPRIORITYR3, 4, u64::from(u32::MAX))
            .unwrap();
        assert_eq!(
            distributor.mmio.words[GICD_IPRIORITYR3 as usize / 4],
            0x11ff_ffff
        );
        distributor.mmio.words[GICD_SGIR as usize / 4] = 0;
        distributor.write(GICD_SGIR, 4, 0x0001_000f).unwrap();
        assert_eq!(distributor.mmio.words[GICD_SGIR as usize / 4], 0);
        assert_eq!(
            distributor.write(GICD_SGIR + 2, 2, 0x0001),
            Err(DeviceError::AccessSize)
        );
        assert_eq!(distributor.mmio.words[GICD_SGIR as usize / 4], 0);
    }
}
