//! Four pinned virtual CPUs. Firmware always receives the EL2 trampoline.
//!
//! EL2 keeps its data cache off. Use only atomic loads/stores, never exclusive
//! read-modify-write instructions that require an external memory-system monitor.
use crate::{
    IPA_LIMIT,
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
    stage2::RAM_BASE,
};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};

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

struct Cpu {
    state: AtomicU8,
    physical_started: AtomicBool,
    entry: AtomicU64,
    context: AtomicU64,
}

impl Cpu {
    const fn new(boot_cpu: bool) -> Self {
        Self {
            state: AtomicU8::new(if boot_cpu { ON } else { OFF }),
            physical_started: AtomicBool::new(boot_cpu),
            entry: AtomicU64::new(0),
            context: AtomicU64::new(0),
        }
    }
}

// Each participant owns one slot. Bit 0 means choosing; the other bits hold its
// Bakery number. Sequentially consistent accesses order the selection protocol.
// This is Lamport's algorithm, independently implemented using loads/stores.
// TF-A uses Bakery locks for its cache-off power-state paths for the same reason:
// https://trustedfirmware-a.readthedocs.io/en/latest/design/firmware-design.html#runtime-services-initialization
struct Claims {
    slots: [AtomicU64; CPU_COUNT],
}

impl Claims {
    const fn new() -> Self {
        Self {
            slots: [const { AtomicU64::new(0) }; CPU_COUNT],
        }
    }

    fn enter(&self, caller: usize) -> Claim<'_> {
        let number = loop {
            self.slots[caller].store(1, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            let maximum = self
                .slots
                .iter()
                .map(|slot| slot.load(Ordering::SeqCst) >> 1)
                .max()
                .unwrap_or(0);
            if maximum == u64::MAX >> 1 {
                // Withdraw before retrying so overflow cannot block a chooser.
                self.slots[caller].store(0, Ordering::SeqCst);
                core::hint::spin_loop();
                continue;
            }
            let number = maximum + 1;
            self.slots[caller].store(number << 1, Ordering::SeqCst);
            break number;
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
        Claim(&self.slots[caller])
    }
}

struct Claim<'a>(&'a AtomicU64);
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::SeqCst);
    }
}

pub struct Machine {
    cpus: [Cpu; CPU_COUNT],
    claims: Claims,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub const fn new() -> Self {
        Self {
            cpus: [
                Cpu::new(true),
                Cpu::new(false),
                Cpu::new(false),
                Cpu::new(false),
            ],
            claims: Claims::new(),
        }
    }

    /// Mint exactly one non-cloneable participant per pinned physical CPU.
    /// The mutable borrow prevents another set while these participants exist.
    pub fn split(&mut self) -> [Participant<'_>; CPU_COUNT] {
        let machine: &Self = self;
        core::array::from_fn(|caller| Participant { machine, caller })
    }
}

/// Exclusive ownership of one caller slot and that CPU's launch consumer.
/// Runtime operations need a mutable borrow, so a participant cannot re-enter.
pub struct Participant<'a> {
    machine: &'a Machine,
    caller: usize,
}

impl Participant<'_> {
    pub fn affinity(&self, target: usize) -> i64 {
        let Some(cpu) = self.machine.cpus.get(target) else {
            return INVALID_PARAMS;
        };
        let state = cpu.state.load(Ordering::Acquire);
        i64::from(if state == CLAIMED { PENDING } else { state })
    }

    /// Only the caller that claims OFF may publish a new context or boot hardware.
    /// No launch is visible until firmware has accepted its EL2 entry.
    pub fn request(
        &mut self,
        target: usize,
        launch: Launch,
        boot_physical: impl FnOnce() -> i64,
    ) -> i64 {
        let Some(cpu) = self.machine.cpus.get(target) else {
            return INVALID_PARAMS;
        };
        if !guest_entry(launch.entry) {
            return INVALID_PARAMS;
        }
        {
            let _claim = self.machine.claims.enter(self.caller);
            match cpu.state.load(Ordering::Acquire) {
                OFF => cpu.state.store(CLAIMED, Ordering::Release),
                ON => return ALREADY_ON,
                _ => return ON_PENDING,
            }
        }
        // Release the Bakery lock before calling firmware. Other callers may
        // observe CLAIMED or claim another CPU while this physical boot runs.
        cpu.entry.store(launch.entry, Ordering::Relaxed);
        cpu.context.store(launch.context, Ordering::Relaxed);
        if !cpu.physical_started.load(Ordering::Acquire) {
            let status = boot_physical();
            if status != 0 {
                cpu.state.store(OFF, Ordering::Release);
                return status;
            }
            cpu.physical_started.store(true, Ordering::Release);
        }
        cpu.state.store(PENDING, Ordering::Release);
        0
    }

    /// The target calls this only after its vectors, EL2 mappings and Stage-2 exist.
    pub fn take_launch(&mut self) -> Option<Launch> {
        let cpu = &self.machine.cpus[self.caller];
        if cpu.state.load(Ordering::Acquire) != PENDING {
            return None;
        }
        // Only this non-cloneable participant consumes its CPU's launch. Read
        // the context before ON permits the next power-off/CPU_ON generation.
        let launch = Launch {
            entry: cpu.entry.load(Ordering::Relaxed),
            context: cpu.context.load(Ordering::Relaxed),
        };
        cpu.state.store(ON, Ordering::Release);
        Some(launch)
    }

    /// The physical CPU remains in EL2, ready for a later virtual CPU_ON.
    pub fn power_off(&mut self) {
        self.machine.cpus[self.caller]
            .state
            .store(OFF, Ordering::Release);
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
