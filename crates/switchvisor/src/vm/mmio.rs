//! Guest MMIO dispatch and access policy for EL2-owned physical resources.
use super::console;
use crate::usb;
use core::arch::asm;
use switchvisor::{drivers::Mmio, mc, mmio::Access, vdev::usb_ownership as ownership};

pub fn emulate(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    console::emulate_uart(esr, far, hpfar, registers)
        || super::network::emulate(esr, far, hpfar, registers)
        || super::interrupt::emulate(esr, far, hpfar, registers)
        || usb(esr, far, hpfar, registers)
        || mc(esr, far, hpfar, registers)
}

fn usb(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    if !usb::enabled() {
        return false;
    }
    for (base, size) in ownership::CONTROLLERS {
        if let Some(access) = Access::decode_region(esr, far, hpfar, base, size) {
            if !access.write {
                access.load_data(0, registers);
            }
            return true; // Absent guest USB bus: no physical read/write or DMA pointers.
        }
    }
    for base in [ownership::CAR, ownership::PMC_PAGE, switchvisor::mc::BASE] {
        let Some(access) = Access::decode_region(esr, far, hpfar, base, 4096) else {
            continue;
        };
        if access.size != 4 {
            return false;
        }
        let address = base + access.offset;
        if base == switchvisor::mc::BASE && address != ownership::DEV_ASID {
            return false;
        }
        // Serialize shared-register RMWs across the four pinned guest CPUs.
        unsafe {
            usb::with_io(|hardware| {
                if access.write {
                    let current = if ownership::reads_current(address) {
                        hardware.read32(address)
                    } else {
                        0
                    };
                    if let Some(value) =
                        ownership::write(address, access.store_value(registers), current)
                    {
                        hardware.write32(address, value);
                        hardware.barrier();
                    }
                } else {
                    let value = if address == ownership::DEV_ASID {
                        0
                    } else {
                        hardware.read32(address)
                    };
                    access.load_value(value, registers);
                }
            });
        }
        return true;
    }
    false
}

fn mc(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    let Some(access) = Access::decode(esr, far, hpfar) else {
        return false;
    };
    // Decode and validate the entire access before any physical MMIO operation.
    let address = (mc::BASE + access.offset) as *mut u32;
    unsafe {
        asm!("dmb sy", options(nostack));
        if access.write {
            // The advertised reservation stays read-only, like a locked carveout.
            if mc::read_override(access.offset).is_none() {
                core::ptr::write_volatile(address, access.store_value(registers));
            }
        } else {
            let value = mc::read_override(access.offset)
                .unwrap_or_else(|| core::ptr::read_volatile(address));
            access.load_value(value, registers);
        }
        asm!("dmb sy", options(nostack));
    }
    true
}
