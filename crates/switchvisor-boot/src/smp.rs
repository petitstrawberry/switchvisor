use core::arch::asm;
use switchvisor_core::psci::{self, CPU_COUNT, Cpu, Launch};

static CPUS: [Cpu; CPU_COUNT] = [const { Cpu::new() }; CPU_COUNT];

unsafe extern "C" {
    fn secondary_el2_entry();
    fn park_vcpu() -> !;
}

pub fn initialize() {
    CPUS[0].initialize_boot_cpu();
}

fn index() -> usize {
    let mpidr: u64;
    unsafe {
        asm!("mrs {mpidr}, mpidr_el1", mpidr = out(reg) mpidr, options(nomem, nostack));
    }
    (mpidr & 0xff) as usize
}

pub fn handle(registers: &mut [u64; 31]) -> bool {
    let function = registers[0] as u32;
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
            let result = CPUS[target].request(launch, || {
                let mut native = [0u64; 31];
                native[0] = u64::from(psci::CPU_ON64);
                native[1] = target as u64;
                native[2] = secondary_el2_entry as *const () as u64;
                native[3] = target as u64;
                // Firmware never receives the guest entry or context directly.
                unsafe { super::forward_smc(native.as_mut_ptr()) };
                native[0] as i64
            });
            unsafe {
                asm!("dsb sy", "sev", options(nostack));
            }
            result
        }
        psci::CPU_OFF => {
            CPUS[index()].power_off();
            unsafe {
                asm!("dsb sy", "sev", options(nostack));
                park_vcpu()
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
                psci::cpu_id(argument(1)).map_or(psci::INVALID_PARAMS, |cpu| CPUS[cpu].affinity())
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
    unsafe {
        super::mmu::enable();
        super::stage2::enable();
    }
    rust_cpu_dispatch()
}

#[unsafe(no_mangle)]
extern "C" fn rust_cpu_dispatch() -> ! {
    let cpu = &CPUS[index()];
    loop {
        if let Some(launch) = cpu.take_launch() {
            let registers = [launch.context, 0, 0, 0, 0, 0, 0, 0];
            unsafe { super::enter_payload(registers.as_ptr(), launch.entry, 0) }
        }
        unsafe {
            asm!("wfe", options(nomem, nostack));
        }
    }
}
