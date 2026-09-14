//! Host-only C bridge to the production Rust memory-discovery policy.
#![no_std]

use switchvisor_core::mc;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn sv_mc_read(offset: u64, physical: u32) -> u32 {
    mc::read_override(offset).unwrap_or(physical)
}

/// The caller supplies the complete, aligned 4 KiB MC register fixture.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sv_mc_valid(registers: *const u32) -> i32 {
    i32::from(mc::validate(|offset| unsafe { registers.add((offset / 4) as usize).read() }).is_ok())
}
