//! Physical memory-geometry check before the resident image is copied.
use switchvisor::mc;

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
