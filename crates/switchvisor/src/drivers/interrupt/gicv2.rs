//! GICv2 hardware-assisted interrupt forwarding for one pinned vCPU.
//!
//! Physical PPIs and SPIs use hardware List Register entries. Physical SGIs
//! use software entries so their source CPU can be preserved until guest EOI.

use crate::drivers::{Driver, InterruptController, Mmio};

pub const MAINTENANCE_IRQ: u32 = 25;
pub const DEBUG_SGI: u32 = 15;
pub const SPURIOUS_IRQ: u32 = 1023;

const GICD_CTLR: u64 = 0x000;
const GICD_IGROUPR0: u64 = 0x080;
const GICD_ISENABLER0: u64 = 0x100;
const GICD_ICENABLER0: u64 = 0x180;
const GICD_ISPENDR0: u64 = 0x200;
const GICD_ICPENDR0: u64 = 0x280;
const GICD_ICACTIVER0: u64 = 0x380;
const GICD_IPRIORITYR0: u64 = 0x400;
const GICD_IPRIORITYR6: u64 = 0x418;
const GICD_ITARGETSR0: u64 = 0x800;
const GICD_ICFGR0: u64 = 0xc00;
const GICD_ICFGR1: u64 = 0xc04;
const GICD_SGIR: u64 = 0xf00;
const GICC_CTLR: u64 = 0x000;
const GICC_PMR: u64 = 0x004;
const GICC_BPR: u64 = 0x008;
const GICC_IAR: u64 = 0x00c;
const GICC_EOIR: u64 = 0x010;
const GICC_RPR: u64 = 0x014;
const GICC_ABPR: u64 = 0x01c;
const GICC_IIDR: u64 = 0x0fc;
const GICC_DIR: u64 = 0x1000;
const GICH_HCR: u64 = 0x000;
const GICH_VTR: u64 = 0x004;
const GICH_VMCR: u64 = 0x008;
const GICH_EISR0: u64 = 0x020;
const GICH_EISR1: u64 = 0x024;
const GICH_ELRSR0: u64 = 0x030;
const GICH_ELRSR1: u64 = 0x034;
const GICH_APR: u64 = 0x0f0;
const GICH_LR0: u64 = 0x100;

const GICC_ENABLE: u32 = 1;
const GICC_EOI_MODE_NS: u32 = 1 << 9;
const GICH_ENABLE: u32 = 1;
const GICH_UNDERFLOW_ENABLE: u32 = 1 << 1;
const LR_EOI: u32 = 1 << 19;
const LR_PRIORITY_SHIFT: u32 = 23;
const LR_PENDING: u32 = 1 << 28;
const LR_HARDWARE: u32 = 1 << 31;
const IRQ_ID_MASK: u32 = 0x3ff;
const SOURCE_CPU_MASK: u32 = 7 << 10;
const MAINTENANCE_CONFIG_MASK: u32 = 3 << 18;

// 128 source-specific SGIs followed by one slot for each architectural INTID.
// This is touched only when every hardware LR is occupied.
const SGI_SLOTS: usize = 16 * 8;
const PENDING_SLOTS: usize = SGI_SLOTS + 1020;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub distributor: u64,
    pub distributor_size: u64,
    pub cpu: u64,
    pub cpu_size: u64,
    pub hypervisor: u64,
    pub hypervisor_size: u64,
    pub virtual_cpu: u64,
    pub virtual_cpu_size: u64,
}

impl Layout {
    pub const TEGRA210: Self = Self {
        distributor: 0x5004_1000,
        distributor_size: 0x1000,
        cpu: 0x5004_2000,
        cpu_size: 0x2000,
        hypervisor: 0x5004_4000,
        hypervisor_size: 0x2000,
        virtual_cpu: 0x5004_6000,
        virtual_cpu_size: 0x2000,
    };
    pub const QEMU_VIRT: Self = Self {
        distributor: 0x0800_0000,
        distributor_size: 0x1_0000,
        cpu: 0x0801_0000,
        cpu_size: 0x1_0000,
        hypervisor: 0x0803_0000,
        hypervisor_size: 0x1_0000,
        virtual_cpu: 0x0804_0000,
        virtual_cpu_size: 0x1_0000,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotGicV2,
    InvalidListRegisterCount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    None,
    Guest,
    Owned(OwnedInterrupt),
    Maintenance,
    DebugKick,
}

/// A physical interrupt acknowledged by EL2 but not yet deactivated or injected.
///
/// The fields remain private so the token can only be completed by `GicV2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnedInterrupt {
    pending: Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    iar: u32,
    priority: u8,
}

impl Pending {
    const EMPTY: Self = Self {
        iar: SPURIOUS_IRQ,
        priority: 0xff,
    };

    const fn valid(self) -> bool {
        self.iar & IRQ_ID_MASK < 1020
    }
}

pub struct GicV2<M> {
    mmio: M,
    layout: Layout,
    list_registers: u8,
    distributor_enabled: bool,
    pending: [Pending; PENDING_SLOTS],
    pending_count: u16,
    owned_interrupt: u32,
    debug_sgi: bool,
    // Original physical IAR values for software SGI LRs awaiting guest EOI.
    lr_iar: [u32; 64],
}

impl<M: Mmio> GicV2<M> {
    pub const fn new(mmio: M, layout: Layout) -> Self {
        Self {
            mmio,
            layout,
            list_registers: 0,
            distributor_enabled: true,
            pending: [Pending::EMPTY; PENDING_SLOTS],
            pending_count: 0,
            owned_interrupt: SPURIOUS_IRQ,
            debug_sgi: false,
            lr_iar: [SPURIOUS_IRQ; 64],
        }
    }

    fn read_dist(&mut self, offset: u64) -> u32 {
        self.mmio
            .read32(self.layout.distributor.wrapping_add(offset))
    }

    fn write_dist(&mut self, offset: u64, value: u32) {
        self.mmio
            .write32(self.layout.distributor.wrapping_add(offset), value);
    }

    fn read_cpu(&mut self, offset: u64) -> u32 {
        self.mmio.read32(self.layout.cpu.wrapping_add(offset))
    }

    fn write_cpu(&mut self, offset: u64, value: u32) {
        self.mmio
            .write32(self.layout.cpu.wrapping_add(offset), value);
    }

    fn read_hyp(&mut self, offset: u64) -> u32 {
        self.mmio
            .read32(self.layout.hypervisor.wrapping_add(offset))
    }

    fn write_hyp(&mut self, offset: u64, value: u32) {
        self.mmio
            .write32(self.layout.hypervisor.wrapping_add(offset), value);
    }

    fn configure_maintenance_interrupt(&mut self) {
        let bit = 1 << MAINTENANCE_IRQ;
        let group = self.read_dist(GICD_IGROUPR0) | bit;
        self.write_dist(GICD_IGROUPR0, group);
        let priorities = self.read_dist(GICD_IPRIORITYR6) & !(0xff << 8);
        self.write_dist(GICD_IPRIORITYR6, priorities);
        let configuration = self.read_dist(GICD_ICFGR1) & !MAINTENANCE_CONFIG_MASK;
        self.write_dist(GICD_ICFGR1, configuration);
        self.write_dist(GICD_ICPENDR0, bit);
        self.write_dist(GICD_ICACTIVER0, bit);
        self.write_dist(GICD_ISENABLER0, bit);
        let control = self.read_dist(GICD_CTLR);
        self.write_dist(GICD_CTLR, control | 1);
    }

    fn slot(iar: u32) -> Option<usize> {
        let id = iar & IRQ_ID_MASK;
        if id < 16 {
            Some((id * 8 + ((iar & SOURCE_CPU_MASK) >> 10)) as usize)
        } else if id < 1020 {
            Some(SGI_SLOTS + id as usize)
        } else {
            None
        }
    }

    fn enqueue(&mut self, pending: Pending) {
        if let Some(slot) = Self::slot(pending.iar) {
            // A PPI/SPI or one source of an SGI cannot be acknowledged twice
            // before deactivation. Keeping the first value preserves its source.
            if !self.pending[slot].valid() {
                self.pending[slot] = pending;
                self.pending_count += 1;
            }
        }
    }

    fn empty_lrs(&mut self) -> u64 {
        let low = u64::from(self.read_hyp(GICH_ELRSR0));
        let high = if self.list_registers > 32 {
            u64::from(self.read_hyp(GICH_ELRSR1)) << 32
        } else {
            0
        };
        let implemented = if self.list_registers == 64 {
            u64::MAX
        } else {
            (1u64 << self.list_registers) - 1
        };
        (low | high) & implemented
    }

    fn eoi_lrs(&mut self) -> u64 {
        u64::from(self.read_hyp(GICH_EISR0))
            | if self.list_registers > 32 {
                u64::from(self.read_hyp(GICH_EISR1)) << 32
            } else {
                0
            }
    }

    fn drain_sgi_eois(&mut self) {
        let eois = self.eoi_lrs();
        for index in 0..usize::from(self.list_registers) {
            if eois & (1u64 << index) != 0 {
                self.write_hyp(GICH_LR0 + index as u64 * 4, 0);
                let iar = core::mem::replace(&mut self.lr_iar[index], SPURIOUS_IRQ);
                if iar & IRQ_ID_MASK < 16 {
                    self.write_cpu(GICC_DIR, iar);
                }
            }
        }
    }

    fn encode_lr(pending: Pending) -> u32 {
        let id = pending.iar & IRQ_ID_MASK;
        // A GICv2 VM uses the legacy virtual Group 0 view even though its
        // physical Non-secure interrupts belong to physical Group 1.
        let mut lr = id | LR_PENDING | u32::from(pending.priority) << LR_PRIORITY_SHIFT;
        if id < 16 {
            lr |= pending.iar & SOURCE_CPU_MASK;
            lr |= LR_EOI;
        } else {
            lr |= LR_HARDWARE | id << 10;
        }
        lr
    }

    fn inject(&mut self, index: usize, pending: Pending) {
        let id = pending.iar & IRQ_ID_MASK;
        self.lr_iar[index] = if id < 16 { pending.iar } else { SPURIOUS_IRQ };
        self.write_hyp(GICH_LR0 + index as u64 * 4, Self::encode_lr(pending));
    }

    fn sgi_in_lr(&self, id: u32) -> bool {
        id < 16
            && self.lr_iar[..usize::from(self.list_registers)]
                .iter()
                .any(|iar| iar & IRQ_ID_MASK == id)
    }

    fn injectable(&self, pending: Pending) -> bool {
        let id = pending.iar & IRQ_ID_MASK;
        id >= 16 || !self.sgi_in_lr(id)
    }

    /// Common delivery takes this path: one ELRSR read and one LR write, with
    /// no pending-array scan while a hardware LR is available.
    fn try_inject(&mut self, pending: Pending) -> bool {
        if !self.distributor_enabled || !self.injectable(pending) {
            return false;
        }
        let empty = self.empty_lrs();
        if empty == 0 {
            return false;
        }
        self.inject(empty.trailing_zeros() as usize, pending);
        true
    }

    fn pop_highest(&mut self) -> Option<Pending> {
        let mut best = None;
        for (index, value) in self.pending.iter().enumerate() {
            if value.valid()
                && self.injectable(*value)
                && best.is_none_or(|current: usize| {
                    (value.priority, value.iar & IRQ_ID_MASK)
                        < (
                            self.pending[current].priority,
                            self.pending[current].iar & IRQ_ID_MASK,
                        )
                })
            {
                best = Some(index);
            }
        }
        best.map(|index| {
            self.pending_count -= 1;
            core::mem::replace(&mut self.pending[index], Pending::EMPTY)
        })
    }

    fn has_injectable_pending(&self) -> bool {
        self.pending_count != 0
            && self
                .pending
                .iter()
                .any(|value| value.valid() && self.injectable(*value))
    }

    fn update_hcr(&mut self) {
        let mut control = 0;
        if self.distributor_enabled {
            control |= GICH_ENABLE;
            if self.has_injectable_pending() {
                control |= GICH_UNDERFLOW_ENABLE;
            }
        }
        self.write_hyp(GICH_HCR, control);
    }

    fn refill(&mut self) {
        if self.distributor_enabled {
            let mut empty = self.empty_lrs();
            while empty != 0 && self.pending_count != 0 {
                let Some(pending) = self.pop_highest() else {
                    break;
                };
                let index = empty.trailing_zeros() as usize;
                self.inject(index, pending);
                empty &= !(1u64 << index);
            }
        }
        self.update_hcr();
    }

    pub fn set_distributor_enabled(&mut self, enabled: bool) {
        if self.distributor_enabled != enabled {
            self.distributor_enabled = enabled;
            self.refill();
            self.mmio.barrier();
        }
    }

    /// Select one physical PPI/SPI for explicit EL2 service before guest delivery.
    pub fn set_owned_interrupt(&mut self, interrupt: Option<u32>) {
        self.owned_interrupt = interrupt
            .filter(|id| (16..1020).contains(id))
            .unwrap_or(SPURIOUS_IRQ);
    }

    pub fn set_debug_sgi(&mut self, enabled: bool) {
        self.debug_sgi = enabled;
    }

    pub fn configure_debug_sgi(&mut self) {
        if !self.debug_sgi {
            return;
        }
        let bit = 1 << DEBUG_SGI;
        let group = self.read_dist(GICD_IGROUPR0) | bit;
        self.write_dist(GICD_IGROUPR0, group);
        let priority_register = GICD_IPRIORITYR0 + u64::from(DEBUG_SGI / 4) * 4;
        let shift = (DEBUG_SGI % 4) * 8;
        let priority = self.read_dist(priority_register) & !(0xff << shift);
        self.write_dist(priority_register, priority);
        // A stop request may race with a secondary's first GIC setup. Keep
        // any pending SGI so it can stop that CPU after its EL1 handoff.
        self.write_dist(GICD_ISENABLER0, bit);
        self.mmio.barrier();
    }

    pub fn send_debug_kick(&mut self, target_mask: u8) {
        if self.debug_sgi && target_mask != 0 {
            self.write_dist(GICD_SGIR, (u32::from(target_mask) << 16) | DEBUG_SGI);
            self.mmio.barrier();
        }
    }

    /// Configure the physical PPI/SPI so EL2 can service it independently from
    /// the guest distributor state. Call once on the boot CPU.
    pub fn configure_owned_interrupt(&mut self, target_mask: u8) {
        let id = self.owned_interrupt;
        if id >= 1020 {
            return;
        }
        let register = u64::from(id / 32) * 4;
        let bit = 1 << (id % 32);
        self.write_dist(GICD_ICENABLER0 + register, bit);

        let group = self.read_dist(GICD_IGROUPR0 + register) | bit;
        self.write_dist(GICD_IGROUPR0 + register, group);

        let byte_register = u64::from(id / 4) * 4;
        let byte_shift = (id % 4) * 8;
        let priority = self.read_dist(GICD_IPRIORITYR0 + byte_register) & !(0xff << byte_shift);
        self.write_dist(GICD_IPRIORITYR0 + byte_register, priority);
        if id >= 32 {
            let targets = self.read_dist(GICD_ITARGETSR0 + byte_register);
            let targets =
                (targets & !(0xff << byte_shift)) | (u32::from(target_mask) << byte_shift);
            self.write_dist(GICD_ITARGETSR0 + byte_register, targets);
        }

        let config_register = u64::from(id / 16) * 4;
        let config_shift = (id % 16) * 2;
        let configuration = self.read_dist(GICD_ICFGR0 + config_register) & !(3 << config_shift);
        self.write_dist(GICD_ICFGR0 + config_register, configuration);
        self.write_dist(GICD_ICPENDR0 + register, bit);
        self.write_dist(GICD_ICACTIVER0 + register, bit);
        self.write_dist(GICD_ISENABLER0 + register, bit);
        let control = self.read_dist(GICD_CTLR);
        self.write_dist(GICD_CTLR, control | 1);
        self.mmio.barrier();
    }

    /// Assert the physical pending bit backing the owned virtual level interrupt.
    pub fn pend_owned_interrupt(&mut self) {
        let id = self.owned_interrupt;
        if id >= 1020 {
            return;
        }
        self.write_dist(GICD_ISPENDR0 + u64::from(id / 32) * 4, 1 << (id % 32));
        self.mmio.barrier();
    }

    /// Complete EL2 service, either deactivating the source or forwarding it.
    pub fn finish_owned_interrupt(&mut self, interrupt: OwnedInterrupt, deliver: bool) {
        let id = interrupt.pending.iar & IRQ_ID_MASK;
        debug_assert_eq!(id, self.owned_interrupt);
        if deliver {
            // Leave another instance pending while the current hardware LR is
            // active. Guest EOI deactivates the LR's physical interrupt; if the
            // virtual UART is still asserted, the pending instance returns to
            // EL2 and is injected again.
            self.write_dist(GICD_ISPENDR0 + u64::from(id / 32) * 4, 1 << (id % 32));
            if self.pending_count != 0 || !self.try_inject(interrupt.pending) {
                self.enqueue(interrupt.pending);
                self.refill();
            }
        } else {
            self.write_cpu(GICC_DIR, interrupt.pending.iar);
        }
        self.mmio.barrier();
    }

    fn complete_owned_interrupt(&mut self, iar: u32) {
        self.write_cpu(GICC_EOIR, iar);
        self.write_cpu(GICC_DIR, iar);
    }
}

impl<M: Mmio> Driver for GicV2<M> {
    type Error = Error;

    fn initialize(&mut self) -> Result<(), Self::Error> {
        if self.read_cpu(GICC_IIDR) & 0x00ff_0fff != 0x0002_043b {
            return Err(Error::NotGicV2);
        }
        let count = (self.read_hyp(GICH_VTR) & 0x3f) + 1;
        if count > 64 {
            return Err(Error::InvalidListRegisterCount);
        }
        self.list_registers = count as u8;
        self.pending.fill(Pending::EMPTY);
        self.pending_count = 0;
        self.lr_iar.fill(SPURIOUS_IRQ);
        for index in 0..count {
            self.write_hyp(GICH_LR0 + u64::from(index) * 4, 0);
        }
        self.write_hyp(GICH_APR, 0);

        // The virtual GICv2 interface presents the guest with the legacy
        // single-group view, so its Enable bit remains virtual Group 0.
        let ctlr = self.read_cpu(GICC_CTLR);
        let pmr = self.read_cpu(GICC_PMR);
        let bpr = self.read_cpu(GICC_BPR) & 7;
        let abpr = self.read_cpu(GICC_ABPR) & 7;
        let vmcr = (ctlr & GICC_ENABLE)
            | (ctlr & ((1 << 4) | GICC_EOI_MODE_NS))
            | (abpr << 18)
            | (bpr << 21)
            | (((pmr >> 3) & 0x1f) << 27);
        self.write_hyp(GICH_VMCR, vmcr);

        self.configure_maintenance_interrupt();
        self.write_cpu(GICC_PMR, 0xff);
        self.write_cpu(GICC_CTLR, ctlr | GICC_ENABLE | GICC_EOI_MODE_NS);
        self.write_hyp(GICH_HCR, GICH_ENABLE);
        self.mmio.barrier();
        Ok(())
    }

    fn poll(&mut self) -> Result<(), Self::Error> {
        self.drain_sgi_eois();
        self.refill();
        self.mmio.barrier();
        Ok(())
    }
}

impl<M: Mmio> InterruptController for GicV2<M> {
    type Event = Event;

    fn take_interrupt(&mut self) -> Self::Event {
        let iar = self.read_cpu(GICC_IAR);
        let id = iar & IRQ_ID_MASK;
        if id >= 1020 {
            return Event::None;
        }
        if id == MAINTENANCE_IRQ {
            self.drain_sgi_eois();
            self.refill();
            // Clear the level source before deactivating its physical PPI.
            self.complete_owned_interrupt(iar);
            self.mmio.barrier();
            return Event::Maintenance;
        }
        if self.debug_sgi && id == DEBUG_SGI {
            self.complete_owned_interrupt(iar);
            self.mmio.barrier();
            return Event::DebugKick;
        }

        let pending = Pending {
            iar,
            priority: ((self.read_cpu(GICC_RPR) >> 3) & 0x1f) as u8,
        };
        // EOImodeNS=1 drops only physical priority. A HW LR lets guest EOI
        // deactivate the PPI/SPI later; software SGIs are handled by EISR.
        self.write_cpu(GICC_EOIR, iar);
        if id == self.owned_interrupt {
            self.mmio.barrier();
            return Event::Owned(OwnedInterrupt { pending });
        }
        if self.pending_count != 0 || !self.try_inject(pending) {
            self.enqueue(pending);
            self.refill();
        }
        self.mmio.barrier();
        Event::Guest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_slots_distinguish_sgi_sources() {
        assert_ne!(GicV2::<Dummy>::slot(1), GicV2::<Dummy>::slot(1 | (2 << 10)));
        assert_eq!(GicV2::<Dummy>::slot(32), Some(SGI_SLOTS + 32));
        assert_eq!(GicV2::<Dummy>::slot(SPURIOUS_IRQ), None);
    }

    #[test]
    fn list_register_encoding_uses_hardware_only_for_ppis_and_spis() {
        let spi = GicV2::<Dummy>::encode_lr(Pending {
            iar: 57,
            priority: 0x12,
        });
        assert_eq!(spi & IRQ_ID_MASK, 57);
        assert_eq!((spi >> 10) & IRQ_ID_MASK, 57);
        assert_ne!(spi & LR_HARDWARE, 0);
        assert_eq!(spi & LR_EOI, 0);

        let sgi = GicV2::<Dummy>::encode_lr(Pending {
            iar: 3 | (2 << 10),
            priority: 4,
        });
        assert_eq!(sgi & IRQ_ID_MASK, 3);
        assert_eq!(sgi & SOURCE_CPU_MASK, 2 << 10);
        assert_eq!(sgi & LR_HARDWARE, 0);
        assert_ne!(sgi & LR_EOI, 0);
    }

    #[test]
    fn sgi_sources_with_the_same_virtual_id_are_serialized() {
        let mut gic = GicV2::new(Dummy, Layout::TEGRA210);
        gic.list_registers = 4;
        gic.lr_iar[0] = 3 | (1 << 10);
        let next_source = Pending {
            iar: 3 | (2 << 10),
            priority: 4,
        };
        assert!(!gic.injectable(next_source));
        gic.enqueue(next_source);
        assert_eq!(gic.pending_count, 1);
        assert_eq!(gic.pop_highest().map(|pending| pending.iar), None);
        assert_eq!(gic.pending_count, 1);
        gic.lr_iar[0] = SPURIOUS_IRQ;
        assert_eq!(
            gic.pop_highest().map(|pending| pending.iar),
            Some(next_source.iar)
        );
        assert_eq!(gic.pending_count, 0);
    }

    #[test]
    fn owned_spi_is_serviced_before_hardware_lr_delivery() {
        const OWNED: u32 = 76;
        const TEST_LAYOUT: Layout = Layout {
            distributor: 0,
            distributor_size: 0x1000,
            cpu: 0x2000,
            cpu_size: 0x2000,
            hypervisor: 0x4000,
            hypervisor_size: 0x2000,
            virtual_cpu: 0x6000,
            virtual_cpu_size: 0x2000,
        };
        let mut gic = GicV2::new(Memory::new(), TEST_LAYOUT);
        gic.list_registers = 1;
        gic.set_owned_interrupt(Some(OWNED));
        gic.mmio.words[(TEST_LAYOUT.cpu + GICC_IAR) as usize / 4] = OWNED;
        gic.mmio.words[(TEST_LAYOUT.cpu + GICC_RPR) as usize / 4] = 0x58;
        gic.mmio.words[(TEST_LAYOUT.hypervisor + GICH_ELRSR0) as usize / 4] = 1;

        let Event::Owned(interrupt) = gic.take_interrupt() else {
            panic!("owned interrupt was forwarded before service");
        };
        assert_eq!(
            gic.mmio.words[(TEST_LAYOUT.cpu + GICC_EOIR) as usize / 4],
            OWNED
        );
        assert_eq!(
            gic.mmio.words[(TEST_LAYOUT.hypervisor + GICH_LR0) as usize / 4],
            0
        );

        gic.finish_owned_interrupt(interrupt, true);
        assert_eq!(
            gic.mmio.words[(TEST_LAYOUT.distributor + GICD_ISPENDR0 + 8) as usize / 4],
            1 << (OWNED % 32)
        );
        let lr = gic.mmio.words[(TEST_LAYOUT.hypervisor + GICH_LR0) as usize / 4];
        assert_eq!(lr & IRQ_ID_MASK, OWNED);
        assert_eq!((lr >> 10) & IRQ_ID_MASK, OWNED);
        assert_ne!(lr & LR_HARDWARE, 0);
        assert_eq!(gic.mmio.words[(TEST_LAYOUT.cpu + GICC_DIR) as usize / 4], 0);
    }

    #[test]
    fn owned_spi_is_configured_for_el2_independently_of_the_guest() {
        const OWNED: u32 = 76;
        const TEST_LAYOUT: Layout = Layout {
            distributor: 0,
            distributor_size: 0x1000,
            cpu: 0x2000,
            cpu_size: 0x2000,
            hypervisor: 0x4000,
            hypervisor_size: 0x2000,
            virtual_cpu: 0x6000,
            virtual_cpu_size: 0x2000,
        };
        let mut gic = GicV2::new(Memory::new(), TEST_LAYOUT);
        gic.set_owned_interrupt(Some(OWNED));
        gic.mmio.words[(GICD_IPRIORITYR0 + 76) as usize / 4] = u32::MAX;
        gic.mmio.words[(GICD_ITARGETSR0 + 76) as usize / 4] = u32::MAX;
        gic.mmio.words[(GICD_ICFGR0 + 16) as usize / 4] = u32::MAX;

        gic.configure_owned_interrupt(1);

        let register = u64::from(OWNED / 32) * 4;
        let bit = 1 << (OWNED % 32);
        assert_ne!(
            gic.mmio.words[(GICD_IGROUPR0 + register) as usize / 4] & bit,
            0
        );
        assert_eq!(
            gic.mmio.words[(GICD_ISENABLER0 + register) as usize / 4],
            bit
        );
        assert_eq!(
            gic.mmio.words[(GICD_IPRIORITYR0 + 76) as usize / 4] & 0xff,
            0
        );
        assert_eq!(
            gic.mmio.words[(GICD_ITARGETSR0 + 76) as usize / 4] & 0xff,
            1
        );
        assert_eq!(
            (gic.mmio.words[(GICD_ICFGR0 + 16) as usize / 4] >> 24) & 3,
            0
        );
    }

    #[test]
    fn private_sgi_is_consumed_by_el2_without_a_guest_list_register() {
        const TEST_LAYOUT: Layout = Layout {
            distributor: 0,
            distributor_size: 0x1000,
            cpu: 0x2000,
            cpu_size: 0x2000,
            hypervisor: 0x4000,
            hypervisor_size: 0x2000,
            virtual_cpu: 0x6000,
            virtual_cpu_size: 0x2000,
        };
        let mut gic = GicV2::new(Memory::new(), TEST_LAYOUT);
        gic.list_registers = 1;
        gic.set_debug_sgi(true);
        gic.configure_debug_sgi();
        assert_ne!(
            gic.mmio.words[GICD_IGROUPR0 as usize / 4] & (1 << DEBUG_SGI),
            0
        );
        gic.send_debug_kick(0b1010);
        assert_eq!(
            gic.mmio.words[GICD_SGIR as usize / 4],
            (0b1010 << 16) | DEBUG_SGI
        );
        let iar = DEBUG_SGI | (2 << 10);
        gic.mmio.words[(TEST_LAYOUT.cpu + GICC_IAR) as usize / 4] = iar;
        assert_eq!(gic.take_interrupt(), Event::DebugKick);
        assert_eq!(
            gic.mmio.words[(TEST_LAYOUT.cpu + GICC_EOIR) as usize / 4],
            iar
        );
        assert_eq!(
            gic.mmio.words[(TEST_LAYOUT.cpu + GICC_DIR) as usize / 4],
            iar
        );
        assert_eq!(
            gic.mmio.words[(TEST_LAYOUT.hypervisor + GICH_LR0) as usize / 4],
            0
        );
    }

    struct Dummy;
    impl Mmio for Dummy {
        fn read32(&mut self, _: u64) -> u32 {
            0
        }
        fn write32(&mut self, _: u64, _: u32) {}
        fn barrier(&mut self) {}
    }

    struct Memory {
        words: [u32; 8192],
    }

    impl Memory {
        const fn new() -> Self {
            Self { words: [0; 8192] }
        }
    }

    impl Mmio for Memory {
        fn read32(&mut self, address: u64) -> u32 {
            self.words[address as usize / 4]
        }
        fn write32(&mut self, address: u64, value: u32) {
            self.words[address as usize / 4] = value;
        }
        fn barrier(&mut self) {}
    }
}
