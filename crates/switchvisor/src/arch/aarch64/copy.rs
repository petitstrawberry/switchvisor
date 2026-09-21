//! Bounded volatile access to guest-owned Normal RAM without Rust references
//! into memory that another CPU can mutate. Never widen beyond the given range.

/// # Safety
/// `address` must name readable Normal RAM for `output.len()` bytes, disjoint
/// from `output`. The caller provides cache maintenance and memory ordering.
pub unsafe fn read(address: *const u8, output: &mut [u8]) {
    let mut cursor = 0;
    unsafe {
        while cursor < output.len() && (address.add(cursor) as usize & 7) != 0 {
            output[cursor] = address.add(cursor).read_volatile();
            cursor += 1;
        }
        while output.len() - cursor >= 8 {
            let word = address.add(cursor).cast::<u64>().read_volatile();
            output
                .as_mut_ptr()
                .add(cursor)
                .cast::<u64>()
                .write_unaligned(word);
            cursor += 8;
        }
        while cursor < output.len() {
            output[cursor] = address.add(cursor).read_volatile();
            cursor += 1;
        }
    }
}

/// # Safety
/// `address` must name writable Normal RAM for `bytes.len()` bytes, disjoint
/// from `bytes`. The caller provides cache maintenance and memory ordering.
pub unsafe fn write(address: *mut u8, bytes: &[u8]) {
    let mut cursor = 0;
    unsafe {
        while cursor < bytes.len() && (address.add(cursor) as usize & 7) != 0 {
            address.add(cursor).write_volatile(bytes[cursor]);
            cursor += 1;
        }
        while bytes.len() - cursor >= 8 {
            let word = bytes.as_ptr().add(cursor).cast::<u64>().read_unaligned();
            address.add(cursor).cast::<u64>().write_volatile(word);
            cursor += 8;
        }
        while cursor < bytes.len() {
            address.add(cursor).write_volatile(bytes[cursor]);
            cursor += 1;
        }
    }
}
