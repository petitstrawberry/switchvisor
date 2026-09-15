use crate::arch::aarch64 as cpu;
use core::{
    arch::asm,
    cell::UnsafeCell,
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};
use switchvisor::psci::{self, CPU_COUNT, Launch, Machine, Participant};

struct SharedMachine(UnsafeCell<Machine>);
// CPU0 mints the participants before any guest or secondary starts. After that
// Machine is shared only through its atomic loads/stores, never mutated wholesale.
unsafe impl Sync for SharedMachine {}
static MACHINE: SharedMachine = SharedMachine(UnsafeCell::new(Machine::new()));

struct PerCpu(UnsafeCell<Option<Participant<'static>>>);
// Each physical CPU exclusively accesses its own non-cloneable participant,
// with DAIF masked and the exception re-entry guard active. CPU0 populates all
// slots before handing off to EL1 or issuing native CPU_ON.
unsafe impl Sync for PerCpu {}
static PARTICIPANTS: [PerCpu; CPU_COUNT] = [const { PerCpu(UnsafeCell::new(None)) }; CPU_COUNT];

// Each CPU writes its own diagnostic slot using ordinary loads/stores.
static STAGES: [AtomicU64; CPU_COUNT] = [const { AtomicU64::new(0) }; CPU_COUNT];
static SMCS: [AtomicU64; CPU_COUNT] = [const { AtomicU64::new(0) }; CPU_COUNT];

#[repr(u64)]
pub enum Stage {
    Entry = 1,
    Mmu = 2,
    Stage2 = 3,
    Wait = 4,
    Guest = 5,
    Smc = 6,
    Off = 7,
}

pub fn record(stage: Stage) {
    STAGES[index()].store(stage as u64, Ordering::Release);
}

pub fn diagnostics(screen: &mut impl Write) {
    let _ = writeln!(screen, "CPU = {}", index());
    for cpu in 0..CPU_COUNT {
        let _ = writeln!(
            screen,
            "C{cpu} STAGE = {} SMC = {:08x}",
            STAGES[cpu].load(Ordering::Acquire),
            SMCS[cpu].load(Ordering::Acquire)
        );
    }
}

pub fn initialize() {
    let machine = unsafe { &mut *MACHINE.0.get() };
    for (slot, participant) in PARTICIPANTS.iter().zip(machine.split()) {
        unsafe { *slot.0.get() = Some(participant) };
    }
    record(Stage::Stage2);
}

fn index() -> usize {
    let mpidr: u64;
    unsafe {
        asm!("mrs {mpidr}, mpidr_el1", mpidr = out(reg) mpidr, options(nomem, nostack));
    }
    (mpidr & 0xff) as usize
}

fn with_cpu<T>(operation: impl FnOnce(&mut Participant<'static>) -> T) -> T {
    let participant = unsafe { &mut *PARTICIPANTS[index()].0.get() };
    operation(participant.as_mut().expect("CPU participant initialized"))
}

pub fn handle(registers: &mut [u64; 31]) -> bool {
    let function = registers[0] as u32;
    SMCS[index()].store(u64::from(function), Ordering::Release);
    record(Stage::Smc);
    let status = match function {
        psci::CPU_ON32 | psci::CPU_ON64 => {
            let argument = |i: usize| {
                if function == psci::CPU_ON32 {
                    registers[i] as u32 as u64
                } else {
                    registers[i]
                }
            };
            let Some(target) = psci::cpu_id(argument(1)) else {
                registers[0] = psci::INVALID_PARAMS as u64;
                return true;
            };
            let launch = Launch {
                entry: argument(2),
                context: argument(3),
            };
            let result = with_cpu(|cpu| {
                cpu.request(target, launch, || {
                    let mut native = [0u64; 31];
                    native[0] = u64::from(psci::CPU_ON64);
                    native[1] = target as u64;
                    native[2] = cpu::secondary_el2_entry as *const () as u64;
                    native[3] = target as u64;
                    // Firmware never receives the guest entry or context directly.
                    unsafe { cpu::forward_smc(native.as_mut_ptr()) };
                    native[0] as i64
                })
            });
            unsafe {
                asm!("dsb sy", "sev", options(nostack));
            }
            result
        }
        psci::CPU_OFF => {
            with_cpu(Participant::power_off);
            record(Stage::Off);
            unsafe {
                asm!("dsb sy", "sev", options(nostack));
                cpu::park_vcpu()
            }
        }
        psci::AFFINITY32 | psci::AFFINITY64 => {
            let argument = |i: usize| {
                if function == psci::AFFINITY32 {
                    registers[i] as u32 as u64
                } else {
                    registers[i]
                }
            };
            if argument(2) != 0 {
                psci::INVALID_PARAMS
            } else {
                psci::cpu_id(argument(1)).map_or(psci::INVALID_PARAMS, |target| {
                    with_cpu(|cpu| cpu.affinity(target))
                })
            }
        }
        psci::FEATURES => {
            let Some(status) = psci::feature(registers[1] as u32) else {
                return false;
            };
            status
        }
        _ if psci::suspend(function) => psci::NOT_SUPPORTED,
        _ => return false,
    };
    registers[0] = status as u64;
    true
}

#[unsafe(no_mangle)]
extern "C" fn rust_secondary() -> ! {
    // No BSS clearing or table construction occurs on a secondary CPU.
    record(Stage::Entry);
    unsafe {
        cpu::mmu::enable();
        record(Stage::Mmu);
        cpu::stage2::enable();
    }
    record(Stage::Stage2);
    rust_cpu_dispatch()
}

#[unsafe(no_mangle)]
extern "C" fn rust_cpu_dispatch() -> ! {
    record(Stage::Wait);
    loop {
        if let Some(launch) = with_cpu(Participant::take_launch) {
            if cpu::interrupt::initialize().is_err() {
                cpu::park()
            }
            let registers = [launch.context, 0, 0, 0, 0, 0, 0, 0];
            record(Stage::Guest);
            unsafe { cpu::enter_payload(registers.as_ptr(), launch.entry, 0) }
        }
        unsafe {
            asm!("wfe", options(nomem, nostack));
        }
    }
}
