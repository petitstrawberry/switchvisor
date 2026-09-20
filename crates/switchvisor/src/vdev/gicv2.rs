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
const GICD_IPRIORITYR6: u64 = 0x418;
const GICD_ITARGETSR0: u64 = 0x800;
const GICD_ICFGR0: u64 = 0xc00;
const GICD_ICFGR1: u64 = 0xc04;

const MAINTENANCE_BIT: u32 = 1 << MAINTENANCE_IRQ;
const MAINTENANCE_PRIORITY: u32 = 0xff << 8;
const MAINTENANCE_CONFIG: u32 = 3 << 18;
pub const DEFAULT_OWNED_PRIORITY: u8 = 0xa0;

pub struct Distributor<M> {
    mmio: M,
    layout: Layout,
    guest_control: u32,
    owned_interrupt: [u32; 2],
    owned_interrupt_enabled: [bool; 2],
    owned_interrupt_priority: [u8; 2],
}

impl<M: Mmio> Distributor<M> {
    pub const fn new(mmio: M, layout: Layout) -> Self {
        Self {
            mmio,
            layout,
            guest_control: 0,
            owned_interrupt: [SPURIOUS_IRQ; 2],
            owned_interrupt_enabled: [false; 2],
            owned_interrupt_priority: [DEFAULT_OWNED_PRIORITY; 2],
        }
    }

    pub fn set_owned_interrupt(&mut self, interrupt: Option<u32>) {
        self.owned_interrupt[0] = interrupt
            .filter(|id| (32..1020).contains(id))
            .unwrap_or(SPURIOUS_IRQ);
        self.owned_interrupt_enabled[0] = false;
        self.owned_interrupt_priority[0] = DEFAULT_OWNED_PRIORITY;
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
        self.owned_interrupt_enabled[0]
    }

    pub fn owned_interrupt_priority(&self) -> u8 {
        self.owned_interrupt_priority[0]
    }

    pub fn network_interrupt_priority(&self) -> u8 {
        self.owned_interrupt_priority[1]
    }

    pub fn set_network_interrupt(&mut self, interrupt: Option<u32>) {
        self.owned_interrupt[1] = interrupt
            .filter(|id| (32..1020).contains(id))
            .unwrap_or(SPURIOUS_IRQ);
        self.owned_interrupt_enabled[1] = false;
        self.owned_interrupt_priority[1] = DEFAULT_OWNED_PRIORITY;
    }
    pub fn network_interrupt_enabled(&self) -> bool {
        self.owned_interrupt_enabled[1]
    }
    fn word_mask(id: u32, offset: u64, base: u64) -> u32 {
        if id < 1020 && offset == base + u64::from(id / 32) * 4 {
            1 << (id % 32)
        } else {
            0
        }
    }
    fn owned_word_mask(&self, offset: u64, base: u64) -> u32 {
        self.owned_interrupt
            .iter()
            .fold(0, |mask, &id| mask | Self::word_mask(id, offset, base))
    }
    fn owned_byte_mask(&self, offset: u64, base: u64) -> u32 {
        self.owned_interrupt.iter().fold(0, |mask, &id| {
            mask | if id < 1020 && offset == base + u64::from(id / 4) * 4 {
                0xff << ((id % 4) * 8)
            } else {
                0
            }
        })
    }
    fn owned_config_mask(&self, offset: u64) -> u32 {
        self.owned_interrupt.iter().fold(0, |mask, &id| {
            mask | if id < 1020 && offset == GICD_ICFGR0 + u64::from(id / 16) * 4 {
                3 << ((id % 16) * 2)
            } else {
                0
            }
        })
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
        maintenance
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
            for (index, &id) in self.owned_interrupt.iter().enumerate() {
                if self.owned_interrupt_enabled[index] {
                    value |= Self::word_mask(id, offset, GICD_ISENABLER0)
                        | Self::word_mask(id, offset, GICD_ICENABLER0);
                }
            }
        }
        for (index, &id) in self.owned_interrupt.iter().enumerate() {
            if id < 1020 && offset == GICD_IPRIORITYR0 + u64::from(id / 4) * 4 {
                let shift = (id % 4) * 8;
                value = (value & !(0xff << shift))
                    | (u32::from(self.owned_interrupt_priority[index]) << shift);
            }
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

        for (index, &id) in self.owned_interrupt.iter().enumerate() {
            if value & Self::word_mask(id, word, GICD_ISENABLER0) != 0 {
                self.owned_interrupt_enabled[index] = true;
            }
            if value & Self::word_mask(id, word, GICD_ICENABLER0) != 0 {
                self.owned_interrupt_enabled[index] = false;
            }
            if id < 1020 && word == GICD_IPRIORITYR0 + u64::from(id / 4) * 4 {
                let shift = (id % 4) * 8;
                if mask & (0xff << shift) != 0 {
                    // The virtual GIC implements five priority bits.
                    self.owned_interrupt_priority[index] = ((value >> shift) as u8) & 0xf8;
                }
            }
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
    fn usb_and_network_enables_are_independent_and_physical_bits_are_protected() {
        let mut d = distributor();
        d.set_owned_interrupt(Some(76));
        d.set_network_interrupt(Some(71));
        d.initialize();
        d.write(GICD_ISENABLER0 + 8, 4, 1 << 7).unwrap();
        assert!(d.network_interrupt_enabled());
        assert!(!d.owned_interrupt_enabled());
        assert_eq!(d.read(GICD_ISENABLER0 + 8, 4), Ok(1 << 7));
        assert_eq!(d.mmio.words[(GICD_ISENABLER0 + 8) as usize / 4], 0);
        d.write(GICD_ISENABLER0 + 8, 4, 1 << 12).unwrap();
        d.write(GICD_ICENABLER0 + 8, 4, 1 << 7).unwrap();
        assert!(!d.network_interrupt_enabled());
        assert!(d.owned_interrupt_enabled());
        assert_eq!(d.read(GICD_ISENABLER0 + 8, 4), Ok(1 << 12));
    }

    #[test]
    fn guest_priorities_are_independent_from_physical_service_priorities() {
        let mut d = distributor();
        d.set_owned_interrupt(Some(76));
        d.set_network_interrupt(Some(71));
        let network = GICD_IPRIORITYR0 + 68;
        let console = GICD_IPRIORITYR0 + 76;
        d.mmio.words[network as usize / 4] = 0x0033_2211;
        d.mmio.words[console as usize / 4] = 0x7766_5500;
        assert_eq!(
            d.read(network + 3, 1),
            Ok(u64::from(DEFAULT_OWNED_PRIORITY))
        );
        d.write(network + 3, 1, 0xe3).unwrap();
        assert_eq!(d.network_interrupt_priority(), 0xe0);
        assert_eq!(d.read(network, 4), Ok(0xe033_2211));
        assert_eq!(d.mmio.words[network as usize / 4], 0x0033_2211);
        // Writes to neighboring bytes still pass through, without changing
        // the protected source or the independently shadowed priority.
        d.write(network, 1, 0x48).unwrap();
        assert_eq!(d.read(network, 4), Ok(0xe033_2248));
        d.write(console, 4, 0x4433_2261).unwrap();
        assert_eq!(d.owned_interrupt_priority(), 0x60);
        assert_eq!(d.network_interrupt_priority(), 0xe0);
        assert_eq!(d.read(console, 4), Ok(0x4433_2260));
        assert_eq!(d.mmio.words[console as usize / 4], 0x4433_2200);
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
}
