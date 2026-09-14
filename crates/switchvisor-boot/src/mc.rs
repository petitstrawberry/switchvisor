use core::arch::asm;
use switchvisor_core::{mc, mmio::Access};

// This also runs from the transient copy before relocation. Its call graph must
// remain position independent, use only the head's temporary stack, and access
// no BSS, global pointers, formatting tables or resident data. QEMU exercises
// this exact pre-relocation path with valid and invalid MC geometry.
#[unsafe(no_mangle)]
extern "C" fn mc_placement_valid() -> u64 {
    u64::from(
        mc::validate(|offset| unsafe {
            core::ptr::read_volatile((mc::BASE + offset) as *const u32)
        })
        .is_ok(),
    )
}

pub fn handle(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
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
