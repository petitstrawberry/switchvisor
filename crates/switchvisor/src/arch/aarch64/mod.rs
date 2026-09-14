//! EL2 entry, translation control and cache-off synchronization.
use core::arch::{asm, global_asm};

pub mod mmu;
pub mod stage2;
pub mod sync;

global_asm!(include_str!("entry.S"));

unsafe extern "C" {
    pub fn enter_payload(registers: *const u64, entry: u64, stack_top: u64) -> !;
    pub fn forward_smc(registers: *mut u64);
    pub fn secondary_el2_entry();
    pub fn park_vcpu() -> !;
}

pub fn park() -> ! {
    loop {
        unsafe {
            asm!("wfe", options(nomem, nostack));
        }
    }
}
