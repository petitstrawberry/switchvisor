//! Cache-independent mutual exclusion using only ordered atomic loads/stores.
use core::{
    arch::asm,
    cell::UnsafeCell,
    sync::atomic::{AtomicU64, Ordering, fence},
};

pub struct Mutex<T> {
    slots: [AtomicU64; 4],
    value: UnsafeCell<T>,
}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            slots: [const { AtomicU64::new(0) }; 4],
            value: UnsafeCell::new(value),
        }
    }

    /// DAIF must be masked, CPU affinity must be 0..3, and this CPU must not re-enter.
    /// Entry/vector guards enforce this for all monitor callers. No firmware call
    /// or guest handoff is permitted while a guard is held.
    pub unsafe fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mpidr: u64;
        unsafe {
            asm!("mrs {mpidr}, mpidr_el1", mpidr = out(reg) mpidr, options(nomem, nostack));
        }
        let caller = (mpidr & 0xff) as usize;
        assert!(caller < 4);
        let number = loop {
            self.slots[caller].store(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            let maximum = self
                .slots
                .iter()
                .map(|s| s.load(Ordering::SeqCst) >> 1)
                .max()
                .unwrap_or(0);
            if maximum == u64::MAX >> 1 {
                self.slots[caller].store(0, Ordering::SeqCst);
                core::hint::spin_loop();
                continue;
            }
            self.slots[caller].store((maximum + 1) << 1, Ordering::SeqCst);
            break maximum + 1;
        };
        for (other, slot) in self.slots.iter().enumerate() {
            if other == caller {
                continue;
            }
            while slot.load(Ordering::SeqCst) & 1 != 0 {
                core::hint::spin_loop();
            }
            loop {
                let theirs = slot.load(Ordering::SeqCst) >> 1;
                if theirs == 0 || (theirs, other) >= (number, caller) {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        struct Guard<'a>(&'a AtomicU64);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.store(0, Ordering::SeqCst);
            }
        }
        let _guard = Guard(&self.slots[caller]);
        unsafe { f(&mut *self.value.get()) }
    }
}
