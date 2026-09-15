//! Guest GIC distributor pass-through with EL2-owned interrupt protection.

use crate::{
    drivers::{
        Mmio,
        interrupt::gicv2::{Layout, MAINTENANCE_IRQ},
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
const GICD_IPRIORITYR6: u64 = 0x418;
const GICD_ICFGR1: u64 = 0xc04;

const OWNED_BIT: u32 = 1 << MAINTENANCE_IRQ;
const OWNED_PRIORITY: u32 = 0xff << 8;
const OWNED_CONFIG: u32 = 3 << 18;

pub struct Distributor<M> {
    mmio: M,
    layout: Layout,
    guest_control: u32,
}

impl<M: Mmio> Distributor<M> {
    pub const fn new(mmio: M, layout: Layout) -> Self {
        Self {
            mmio,
            layout,
            guest_control: 0,
        }
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

    fn protected_mask(offset: u64) -> u32 {
        match offset {
            GICD_IGROUPR0 | GICD_ISENABLER0 | GICD_ICENABLER0 | GICD_ISPENDR0 | GICD_ICPENDR0
            | GICD_ISACTIVER0 | GICD_ICACTIVER0 => OWNED_BIT,
            GICD_IPRIORITYR6 => OWNED_PRIORITY,
            GICD_ICFGR1 => OWNED_CONFIG,
            _ => 0,
        }
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
            value &= !OWNED_BIT;
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

        if word == GICD_CTLR {
            self.guest_control = ((self.guest_control & !mask) | value) & 1;
            self.mmio.write32(address, self.guest_control | 1);
        } else if Self::is_write_one(word) {
            self.mmio
                .write32(address, value & !Self::protected_mask(word));
        } else {
            let current = self.mmio.read32(address);
            let merged = (current & !mask) | value;
            let protected = Self::protected_mask(word);
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
            !OWNED_BIT
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

        distributor.mmio.words[GICD_ICFGR1 as usize / 4] = OWNED_CONFIG;
        distributor.write(GICD_ICFGR1, 4, 0).unwrap();
        assert_eq!(
            distributor.mmio.words[GICD_ICFGR1 as usize / 4] & OWNED_CONFIG,
            OWNED_CONFIG
        );
    }
}
