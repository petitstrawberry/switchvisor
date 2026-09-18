//! EL2-owned guest stop state. Each CPU writes only its own context and parked flag.

use core::fmt::Write;
use core::{
    arch::asm,
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use switchvisor::{CPU_COUNT, mc};

use crate::{arch::aarch64::interrupt, platform::tegra210::io::Hardware, vm::vcpu};
use switchvisor::drivers::{Clock, Mmio};

const DISABLED: u8 = 0;
const RUNNING: u8 = 1;
const STOP_REQUESTED: u8 = 2;
const STOPPED: u8 = 3;
const RESUMING: u8 = 4;
const RESUME_NONE: u8 = 0;
const RESUME_CONTINUE: u8 = 1;
const STOP_TIMEOUT_US: u64 = 1_000_000;
const HCR_AMO: u64 = 1 << 5;

#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct GuestContext {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub sp_el1: u64,
    pub pc: u64,
    pub pstate: u64,
    pub v: [u128; 32],
    pub fpcr: u32,
    pub fpsr: u32,
    pub guest_mdscr_el1: u64,
    pub valid: u64,
    pub el2_sp: u64,
    pub resume_mdscr_el1: u64,
    pub resume_mdcr_el2: u64,
    pub saved_guest_ss: u64,
    pub stepping: u64,
}

impl GuestContext {
    const fn new() -> Self {
        Self {
            x: [0; 31],
            sp_el0: 0,
            sp_el1: 0,
            pc: 0,
            pstate: 0,
            v: [0; 32],
            fpcr: 0,
            fpsr: 0,
            guest_mdscr_el1: 0,
            valid: 0,
            el2_sp: 0,
            resume_mdscr_el1: 0,
            resume_mdcr_el2: 0,
            saved_guest_ss: 0,
            stepping: 0,
        }
    }

    pub fn sp(&self) -> u64 {
        if self.pstate & 0xf == 5 {
            self.sp_el1
        } else {
            self.sp_el0
        }
    }

    pub fn set_sp(&mut self, value: u64) {
        if self.pstate & 0xf == 5 {
            self.sp_el1 = value
        } else {
            self.sp_el0 = value
        }
    }
}

const _: () = {
    assert!(core::mem::size_of::<GuestContext>() == 864);
    assert!(core::mem::offset_of!(GuestContext, sp_el0) == 248);
    assert!(core::mem::offset_of!(GuestContext, sp_el1) == 256);
    assert!(core::mem::offset_of!(GuestContext, pc) == 264);
    assert!(core::mem::offset_of!(GuestContext, pstate) == 272);
    assert!(core::mem::offset_of!(GuestContext, v) == 288);
    assert!(core::mem::offset_of!(GuestContext, fpcr) == 800);
    assert!(core::mem::offset_of!(GuestContext, guest_mdscr_el1) == 808);
    assert!(core::mem::offset_of!(GuestContext, valid) == 816);
    assert!(core::mem::offset_of!(GuestContext, el2_sp) == 824);
    assert!(core::mem::offset_of!(GuestContext, resume_mdscr_el1) == 832);
    assert!(core::mem::offset_of!(GuestContext, resume_mdcr_el2) == 840);
    assert!(core::mem::offset_of!(GuestContext, saved_guest_ss) == 848);
    assert!(core::mem::offset_of!(GuestContext, stepping) == 856);
};

#[repr(transparent)]
struct ContextSlot(UnsafeCell<GuestContext>);
unsafe impl Sync for ContextSlot {}

#[used]
#[unsafe(no_mangle)]
static debug_contexts: [ContextSlot; CPU_COUNT] =
    [const { ContextSlot(UnsafeCell::new(GuestContext::new())) }; CPU_COUNT];

static ENABLED: AtomicBool = AtomicBool::new(false);
static WORLD: AtomicU8 = AtomicU8::new(DISABLED);
static ATTACH_REQUESTED: AtomicBool = AtomicBool::new(false);
static DISCONNECT_REQUESTED: AtomicBool = AtomicBool::new(false);
static TARGET_MASK: AtomicU8 = AtomicU8::new(0);
static PARKED: [AtomicBool; CPU_COUNT] = [const { AtomicBool::new(false) }; CPU_COUNT];
static RESUME: [AtomicU8; CPU_COUNT] = [const { AtomicU8::new(RESUME_NONE) }; CPU_COUNT];
static RESUME_READY: [AtomicBool; CPU_COUNT] = [const { AtomicBool::new(false) }; CPU_COUNT];
static STOP_CAUSE: [AtomicU8; CPU_COUNT] = [const { AtomicU8::new(0) }; CPU_COUNT];
static STOP_ESR: [AtomicU64; CPU_COUNT] = [const { AtomicU64::new(0) }; CPU_COUNT];
static STOP_FAR: [AtomicU64; CPU_COUNT] = [const { AtomicU64::new(0) }; CPU_COUNT];
static LAST_STOP_CPU: AtomicU8 = AtomicU8::new(0xff);
static LAST_STOP_CAUSE: AtomicU8 = AtomicU8::new(0);
static LAST_MISSING_MASK: AtomicU8 = AtomicU8::new(0);
static RAM_END: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    fn debug_resume(context: *const GuestContext) -> !;
}

fn cpu_id() -> usize {
    let mpidr: u64;
    unsafe { asm!("mrs {mpidr}, mpidr_el1", mpidr = out(reg) mpidr, options(nomem, nostack)) };
    (mpidr & 0xff) as usize
}

pub fn initialize(enabled: bool) {
    let ram_end = mc::ram_end(|offset| Hardware.read32(mc::BASE + offset)).unwrap_or(0);
    RAM_END.store(ram_end, Ordering::Release);
    ENABLED.store(enabled, Ordering::Release);
    WORLD.store(if enabled { RUNNING } else { DISABLED }, Ordering::SeqCst);
}

pub fn ram_end() -> u64 {
    RAM_END.load(Ordering::Acquire)
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn request_attach() {
    if enabled() {
        ATTACH_REQUESTED.store(true, Ordering::Release);
        if cpu_id() != 0 {
            interrupt::send_debug_kick(1);
        }
    }
}

pub fn attach_requested() -> bool {
    ATTACH_REQUESTED.load(Ordering::Acquire)
}
pub fn cancel_attach() {
    ATTACH_REQUESTED.store(false, Ordering::Release);
}

pub fn request_interrupt() {
    if enabled() {
        ATTACH_REQUESTED.store(true, Ordering::Release);
        if cpu_id() != 0 {
            interrupt::send_debug_kick(1);
        }
    }
}

pub fn request_disconnect() {
    if enabled() {
        DISCONNECT_REQUESTED.store(true, Ordering::Release);
        if cpu_id() != 0 {
            interrupt::send_debug_kick(1);
        }
    }
}

pub fn disconnect_requested() -> bool {
    let requested = DISCONNECT_REQUESTED.load(Ordering::Acquire);
    DISCONNECT_REQUESTED.store(false, Ordering::Release);
    requested
}

pub fn world_stopped() -> bool {
    WORLD.load(Ordering::Acquire) == STOPPED
}

pub fn irq_should_stop() -> bool {
    if !enabled() {
        return false;
    }
    let cpu = cpu_id();
    if cpu == 0 {
        ATTACH_REQUESTED.load(Ordering::Acquire)
            || DISCONNECT_REQUESTED.load(Ordering::Acquire)
            || (1..CPU_COUNT).any(|id| STOP_CAUSE[id].load(Ordering::Acquire) != 0)
    } else {
        WORLD.load(Ordering::Acquire) == STOP_REQUESTED
    }
}

pub fn exception(esr: u64, far: u64) -> bool {
    if !enabled() || !matches!(esr >> 26, 0x2f | 0x30 | 0x32 | 0x34 | 0x3c) {
        return false;
    }
    let cpu = cpu_id();
    STOP_ESR[cpu].store(esr, Ordering::Release);
    STOP_FAR[cpu].store(far, Ordering::Release);
    record_stop(
        cpu,
        match esr >> 26 {
            0x2f => 4,
            0x32 => 3,
            _ => 2,
        },
    );
    true
}

/// MDCR_EL2.TDE also traps guest debug-register accesses. Preserve MDSCR_EL1
/// and provide the specified v1 RAZ/WI behavior for other debug registers.
pub fn sysreg_trap(esr: u64, registers: &mut [u64; 31], elr: u64, spsr: u64) -> Option<bool> {
    if !enabled() || esr >> 26 != 0x18 {
        return None;
    }
    let mdcr: u64;
    unsafe { asm!("mrs {value}, mdcr_el2", value = out(reg) mdcr, options(nomem, nostack)) };
    if mdcr & (1 << 8) == 0 {
        return None;
    }
    let op0 = (esr >> 20) & 3;
    let op1 = (esr >> 14) & 7;
    let crn = (esr >> 10) & 15;
    let crm = (esr >> 1) & 15;
    let op2 = (esr >> 17) & 7;
    if op0 != 2 || crn != 0 {
        return None;
    }
    let rt = ((esr >> 5) & 31) as usize;
    let read = esr & 1 != 0;
    let mdscr = op1 == 0 && crm == 2 && op2 == 2;
    let ctx = unsafe { &mut *debug_contexts[cpu_id()].0.get() };
    if read {
        if rt < 31 {
            registers[rt] = if mdscr { ctx.guest_mdscr_el1 } else { 0 };
        }
    } else if mdscr {
        ctx.guest_mdscr_el1 = if rt < 31 { registers[rt] } else { 0 };
    }
    unsafe {
        asm!("msr elr_el2, {pc}", "msr spsr_el2, {pstate}",
        pc = in(reg) (elr + 4), pstate = in(reg) spsr, options(nostack))
    };
    let step = ctx.stepping != 0;
    if step {
        record_stop(cpu_id(), 3);
    }
    Some(step)
}

pub fn emulated_step() -> bool {
    let cpu = cpu_id();
    if !enabled() || unsafe { (*debug_contexts[cpu].0.get()).stepping == 0 } {
        return false;
    }
    record_stop(cpu, 3);
    true
}

fn record_stop(cpu: usize, cause: u8) {
    STOP_CAUSE[cpu].store(cause, Ordering::Release);
    LAST_STOP_CPU.store(cpu as u8, Ordering::Release);
    LAST_STOP_CAUSE.store(cause, Ordering::Release);
}

#[unsafe(no_mangle)]
extern "C" fn debug_context_ptr() -> *mut GuestContext {
    debug_contexts[cpu_id()].0.get()
}

pub fn context(cpu: usize) -> Option<GuestContext> {
    if cpu >= CPU_COUNT || !PARKED[cpu].load(Ordering::Acquire) {
        return None;
    }
    let ctx = unsafe { &*debug_contexts[cpu].0.get() };
    (ctx.valid != 0).then_some(*ctx)
}

pub fn edit_context<R>(
    cpu: usize,
    edit: impl for<'a> FnOnce(&'a mut GuestContext) -> R,
) -> Option<R> {
    if cpu >= CPU_COUNT
        || WORLD.load(Ordering::Acquire) != STOPPED
        || !PARKED[cpu].load(Ordering::Acquire)
    {
        return None;
    }
    let ctx = unsafe { &mut *debug_contexts[cpu].0.get() };
    (ctx.valid != 0).then(|| edit(ctx))
}

pub fn parked_mask() -> u8 {
    PARKED.iter().enumerate().fold(0, |mask, (cpu, parked)| {
        mask | (u8::from(parked.load(Ordering::Acquire)) << cpu)
    })
}

pub fn target_mask() -> u8 {
    TARGET_MASK.load(Ordering::Acquire)
}
pub fn stop_esr(cpu: usize) -> u64 {
    STOP_ESR[cpu].load(Ordering::Acquire)
}
pub fn stop_far(cpu: usize) -> u64 {
    STOP_FAR[cpu].load(Ordering::Acquire)
}
pub fn stop_cause(cpu: usize) -> u8 {
    STOP_CAUSE[cpu].load(Ordering::Acquire)
}

pub fn status() -> &'static str {
    if !enabled() {
        "disabled"
    } else if !crate::gdb::active() {
        if ATTACH_REQUESTED.load(Ordering::Acquire) {
            "attaching"
        } else {
            "disconnected"
        }
    } else if world_stopped() {
        "stopped"
    } else {
        "running"
    }
}

pub fn diagnostics(screen: &mut impl Write) {
    let _ = writeln!(
        screen,
        "DEBUG={} PARKED={:#x} TARGET={:#x} MISSING={:#x}",
        status(),
        parked_mask(),
        target_mask(),
        last_missing_mask()
    );
    let _ = writeln!(
        screen,
        "LAST STOP CPU={} CAUSE={}",
        LAST_STOP_CPU.load(Ordering::Acquire),
        LAST_STOP_CAUSE.load(Ordering::Acquire)
    );
    for cpu in 0..CPU_COUNT {
        let _ = writeln!(
            screen,
            "D{cpu} CAUSE={} ESR={:016x} FAR={:016x}",
            stop_cause(cpu),
            stop_esr(cpu),
            stop_far(cpu)
        );
    }
}

pub fn last_stop_cpu() -> u8 {
    LAST_STOP_CPU.load(Ordering::Acquire)
}
pub fn last_stop_cause() -> u8 {
    LAST_STOP_CAUSE.load(Ordering::Acquire)
}
pub fn last_missing_mask() -> u8 {
    LAST_MISSING_MASK.load(Ordering::Acquire)
}

pub fn clear_stop_cause(cpu: usize) {
    STOP_CAUSE[cpu].store(0, Ordering::Release);
}

pub fn pending_stop_cpu() -> Option<usize> {
    (0..CPU_COUNT).find(|&cpu| STOP_CAUSE[cpu].load(Ordering::Acquire) != 0)
}

pub fn prepare_resume(cpu: usize, step: bool, breakpoints: bool) -> bool {
    edit_context(cpu, |ctx| {
        ctx.resume_mdscr_el1 = ctx.guest_mdscr_el1 | u64::from(step);
        ctx.resume_mdcr_el2 = if step || breakpoints { 1 << 8 } else { 0 };
        if step {
            ctx.saved_guest_ss = ctx.pstate & (1 << 21);
            ctx.pstate |= 1 << 21;
            ctx.stepping = 1;
        } else {
            ctx.stepping = 0;
        }
    })
    .is_some()
}

/// Each CPU cleans its own guest caches before announcing that its context is stable.
fn clean_guest_cache() {
    cache_all(false);
}

fn invalidate_guest_cache() {
    cache_all(true);
    unsafe { asm!("ic iallu", "dsb sy", "isb", options(nostack)) };
}

fn cache_all(invalidate: bool) {
    let clidr: u64;
    let saved: u64;
    unsafe {
        asm!("mrs {value}, clidr_el1", value = out(reg) clidr, options(nomem, nostack));
        asm!("mrs {value}, csselr_el1", value = out(reg) saved, options(nomem, nostack));
    }
    for level in 0..7u64 {
        let cache_type = (clidr >> (level * 3)) & 7;
        if !matches!(cache_type, 2 | 3 | 4) {
            continue;
        }
        unsafe {
            asm!("msr csselr_el1, {value}", "isb", value = in(reg) (level << 1), options(nostack))
        };
        let ccsidr: u64;
        unsafe {
            asm!("mrs {value}, ccsidr_el1", value = out(reg) ccsidr, options(nomem, nostack))
        };
        let ways = ((ccsidr >> 3) & 0x3ff) + 1;
        let sets = ((ccsidr >> 13) & 0x7fff) + 1;
        let line_shift = (ccsidr & 7) + 4;
        let way_shift = ((ways - 1) as u32).leading_zeros();
        for way in 0..ways {
            for set in 0..sets {
                let operand = (way << way_shift) | (set << line_shift) | (level << 1);
                unsafe {
                    if invalidate {
                        asm!("dc cisw, {value}", value = in(reg) operand, options(nostack));
                    } else {
                        asm!("dc csw, {value}", value = in(reg) operand, options(nostack));
                    }
                }
            }
        }
    }
    unsafe {
        asm!("dsb sy", "msr csselr_el1, {saved}", "isb", saved = in(reg) saved, options(nostack));
    }
}

pub fn guest_entry_allowed() -> bool {
    !enabled() || WORLD.load(Ordering::SeqCst) == RUNNING
}

pub fn set_resume(cpu: usize, run: bool) {
    if cpu < CPU_COUNT {
        RESUME_READY[cpu].store(false, Ordering::Release);
        RESUME[cpu].store(
            if run { RESUME_CONTINUE } else { RESUME_NONE },
            Ordering::Release,
        );
    }
}

pub fn resume_requested(cpu: usize) -> bool {
    RESUME[cpu].load(Ordering::Acquire) == RESUME_CONTINUE
}

pub fn resume_world() {
    WORLD.store(RESUMING, Ordering::SeqCst);
    unsafe { asm!("dsb sy", "sev", options(nostack)) };
    if RESUME[0].load(Ordering::Acquire) == RESUME_CONTINUE {
        invalidate_guest_cache();
        RESUME_READY[0].store(true, Ordering::Release);
    }
    let start = Hardware.now_us();
    while (1..CPU_COUNT).any(|cpu| {
        RESUME[cpu].load(Ordering::Acquire) == RESUME_CONTINUE
            && !RESUME_READY[cpu].load(Ordering::Acquire)
    }) {
        if Hardware.now_us().wrapping_sub(start) >= STOP_TIMEOUT_US {
            break;
        }
        core::hint::spin_loop();
    }
    WORLD.store(RUNNING, Ordering::SeqCst);
    unsafe { asm!("dsb sy", "sev", options(nostack)) };
}

pub fn return_to_guest(cpu: usize) -> ! {
    RESUME[cpu].store(RESUME_NONE, Ordering::Release);
    STOP_CAUSE[cpu].store(0, Ordering::Release);
    if !crate::gdb::active() {
        let ctx = unsafe { &mut *debug_contexts[cpu].0.get() };
        if ctx.stepping != 0 {
            ctx.pstate = (ctx.pstate & !(1 << 21)) | ctx.saved_guest_ss;
            ctx.stepping = 0;
        }
        ctx.resume_mdscr_el1 = ctx.guest_mdscr_el1;
        ctx.resume_mdcr_el2 = 0;
    }
    if !RESUME_READY[cpu].load(Ordering::Acquire) {
        invalidate_guest_cache();
    }
    RESUME_READY[cpu].store(false, Ordering::Release);
    PARKED[cpu].store(false, Ordering::Release);
    // Keep asynchronous guest errors in EL2 while GDB owns the vCPU. If an
    // SError arrives during a step, report it instead of letting the guest's
    // fatal handler park CPU0 and strand the USB control port.
    let hcr: u64;
    unsafe { asm!("mrs {value}, hcr_el2", value = out(reg) hcr, options(nomem, nostack)) };
    let hcr = if crate::gdb::active() {
        hcr | HCR_AMO
    } else {
        hcr & !HCR_AMO
    };
    unsafe { asm!("msr hcr_el2, {value}", "isb", value = in(reg) hcr, options(nostack)) };
    unsafe { debug_resume(debug_contexts[cpu].0.get()) }
}

pub fn stop_world() -> bool {
    ATTACH_REQUESTED.store(false, Ordering::Release);
    WORLD.store(STOP_REQUESTED, Ordering::SeqCst);
    let target = vcpu::guest_mask();
    TARGET_MASK.store(target, Ordering::Release);
    interrupt::send_debug_kick(target & !parked_mask());
    let start = Hardware.now_us();
    loop {
        let pending = target & vcpu::guest_mask() & !parked_mask();
        if pending == 0 {
            LAST_MISSING_MASK.store(0, Ordering::Release);
            WORLD.store(STOPPED, Ordering::SeqCst);
            return true;
        }
        if Hardware.now_us().wrapping_sub(start) >= STOP_TIMEOUT_US {
            LAST_MISSING_MASK.store(pending, Ordering::Release);
            for cpu in 0..CPU_COUNT {
                if PARKED[cpu].load(Ordering::Acquire) {
                    RESUME[cpu].store(RESUME_CONTINUE, Ordering::Release);
                }
            }
            WORLD.store(RUNNING, Ordering::SeqCst);
            unsafe { asm!("dsb sy", "sev", options(nostack)) };
            return false;
        }
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_debug_park() -> ! {
    let cpu = cpu_id();
    // Firmware and the guest can leave either OS debug lock set. The monitor
    // owns debug exceptions while a GDB session is active, so unlock this
    // CPU before programming MDSCR_EL1 for single-step on resume.
    unsafe {
        asm!(
            "msr osdlr_el1, xzr",
            "msr oslar_el1, xzr",
            "isb",
            options(nostack)
        );
    }
    let ctx = unsafe { &mut *debug_contexts[cpu].0.get() };
    if ctx.stepping != 0 {
        ctx.pstate = (ctx.pstate & !(1 << 21)) | ctx.saved_guest_ss;
        ctx.resume_mdscr_el1 = ctx.guest_mdscr_el1;
        ctx.resume_mdcr_el2 = if crate::gdb::breakpoint_count() != 0 {
            1 << 8
        } else {
            0
        };
        ctx.stepping = 0;
    } else if ctx.resume_mdcr_el2 == 0 {
        ctx.guest_mdscr_el1 = ctx.resume_mdscr_el1;
    }
    clean_guest_cache();
    PARKED[cpu].store(true, Ordering::Release);
    unsafe { asm!("dsb sy", "sev", options(nostack)) };
    if cpu != 0 {
        if WORLD.load(Ordering::Acquire) == RUNNING {
            if STOP_CAUSE[cpu].load(Ordering::Acquire) == 0 {
                // A kick from a timed-out stop generation arrived late.
                return_to_guest(cpu);
            }
            interrupt::send_debug_kick(1);
        }
        loop {
            if RESUME[cpu].load(Ordering::Acquire) == RESUME_CONTINUE {
                if WORLD.load(Ordering::Acquire) == RESUMING {
                    invalidate_guest_cache();
                    RESUME_READY[cpu].store(true, Ordering::Release);
                    unsafe { asm!("dsb sy", "sev", options(nostack)) };
                    while WORLD.load(Ordering::Acquire) == RESUMING {
                        unsafe { asm!("wfe", options(nomem, nostack)) };
                    }
                }
                return_to_guest(cpu);
            }
            unsafe { asm!("wfe", options(nomem, nostack)) };
        }
    }
    ATTACH_REQUESTED.store(false, Ordering::Release);
    if !stop_world() {
        crate::gdb::on_stop_timeout();
        return_to_guest(0);
    }
    crate::gdb::monitor();
}
