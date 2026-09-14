//! Four pinned virtual CPUs. Firmware always receives the EL2 trampoline.
//!
//! Atomic state must be accessed through Normal, shareable EL2 mappings.
use crate::{
    IPA_LIMIT,
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
    stage2::RAM_BASE,
};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

pub const CPU_COUNT: usize = 4;
pub const CPU_OFF: u32 = 0x8400_0002;
pub const CPU_ON32: u32 = 0x8400_0003;
pub const CPU_ON64: u32 = 0xc400_0003;
pub const AFFINITY32: u32 = 0x8400_0004;
pub const AFFINITY64: u32 = 0xc400_0004;
pub const FEATURES: u32 = 0x8400_000a;
pub const NOT_SUPPORTED: i64 = -1;
pub const INVALID_PARAMS: i64 = -2;
pub const ALREADY_ON: i64 = -4;
pub const ON_PENDING: i64 = -5;
pub const ON: u8 = 0;
pub const OFF: u8 = 1;
pub const PENDING: u8 = 2;
const CLAIMED: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Launch {
    pub entry: u64,
    pub context: u64,
}

pub struct Cpu {
    state: AtomicU8,
    physical_started: AtomicBool,
    entry: AtomicU64,
    context: AtomicU64,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(OFF),
            physical_started: AtomicBool::new(false),
            entry: AtomicU64::new(0),
            context: AtomicU64::new(0),
        }
    }

    pub fn initialize_boot_cpu(&self) {
        self.physical_started.store(true, Ordering::Relaxed);
        self.state.store(ON, Ordering::Release);
    }

    pub fn affinity(&self) -> i64 {
        let state = self.state.load(Ordering::Acquire);
        i64::from(if state == CLAIMED { PENDING } else { state })
    }

    /// Only the caller that claims OFF may publish a new context or boot hardware.
    /// No launch is visible until firmware has accepted its EL2 entry.
    pub fn request(&self, launch: Launch, boot_physical: impl FnOnce() -> i64) -> i64 {
        if !guest_entry(launch.entry) {
            return INVALID_PARAMS;
        }
        match self
            .state
            .compare_exchange(OFF, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => (),
            Err(ON) => return ALREADY_ON,
            Err(_) => return ON_PENDING,
        }
        self.entry.store(launch.entry, Ordering::Relaxed);
        self.context.store(launch.context, Ordering::Relaxed);
        if !self.physical_started.load(Ordering::Acquire) {
            let status = boot_physical();
            if status != 0 {
                self.state.store(OFF, Ordering::Release);
                return status;
            }
            self.physical_started.store(true, Ordering::Release);
        }
        self.state.store(PENDING, Ordering::Release);
        0
    }

    /// The target calls this only after its vectors, EL2 mappings and Stage-2 exist.
    pub fn take_launch(&self) -> Option<Launch> {
        self.state
            .compare_exchange(PENDING, ON, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Launch {
            entry: self.entry.load(Ordering::Relaxed),
            context: self.context.load(Ordering::Relaxed),
        })
    }

    /// The physical CPU remains in EL2, ready for a later virtual CPU_ON.
    pub fn power_off(&self) {
        self.state.store(OFF, Ordering::Release);
    }
}

pub fn cpu_id(target: u64) -> Option<usize> {
    (target < CPU_COUNT as u64).then_some(target as usize)
}

pub fn guest_entry(entry: u64) -> bool {
    entry >= RAM_BASE
        && entry % 4 == 0
        && entry < IPA_LIMIT
        && !(RESIDENT_BASE..RESIDENT_BASE + RESIDENT_SIZE).contains(&entry)
}

pub fn suspend(function: u32) -> bool {
    matches!(
        function,
        0x8400_0001 | 0xc400_0001 | 0x8400_000e | 0xc400_000e
    )
}

/// None delegates a native feature query to BL31.
pub fn feature(function: u32) -> Option<i64> {
    if suspend(function) {
        Some(NOT_SUPPORTED)
    } else if matches!(
        function,
        CPU_ON32 | CPU_ON64 | CPU_OFF | AFFINITY32 | AFFINITY64
    ) {
        Some(0)
    } else {
        None
    }
}
