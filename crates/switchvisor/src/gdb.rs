//! RSP endpoint for guest-physical, all-stop EL2 debugging.
//!
//! Only CPU0 touches this module's mutable state. The state machine and its
//! packet buffer remain resident while CPU0 is executing the guest.

use core::{
    arch::asm,
    cell::UnsafeCell,
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use gdbstub::{
    common::{Signal, Tid},
    conn::Connection,
    stub::{GdbStub, MultiThreadStopReason, state_machine::GdbStubStateMachine},
    target::{
        Target, TargetError, TargetResult,
        ext::{
            base::{
                BaseOps,
                multithread::{
                    MultiThreadBase, MultiThreadResume, MultiThreadSchedulerLocking,
                    MultiThreadSingleStep,
                },
            },
            breakpoints::{Breakpoints, SwBreakpoint},
        },
    },
};
use gdbstub_arch::aarch64::{AArch64, reg::AArch64CoreRegs};
use switchvisor::{
    CPU_COUNT,
    loader::{GUEST_RAM_BASE, GUEST_RAM_END},
};

use crate::{debug, usb, vm::vcpu};

const MAX_BREAKPOINTS: usize = 64;
const BRK: u32 = 0xd420_0000 | (0x5a5a << 5);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static BREAKPOINT_COUNT: AtomicUsize = AtomicUsize::new(0);
static NEEDS_CLEANUP: AtomicBool = AtomicBool::new(false);

type Machine = GdbStubStateMachine<'static, GdbTarget, GdbConnection>;
struct Cell<T>(UnsafeCell<T>);
// CPU0 is the sole writer; DAIF is masked in the monitor and IRQ paths.
unsafe impl<T> Sync for Cell<T> {}
static TARGET: Cell<GdbTarget> = Cell(UnsafeCell::new(GdbTarget::new()));
static MACHINE: Cell<Option<Machine>> = Cell(UnsafeCell::new(None));
static PACKET: Cell<[u8; 4096]> = Cell(UnsafeCell::new([0; 4096]));

#[derive(Clone, Copy)]
struct Breakpoint {
    address: u64,
    original: [u8; 4],
    active: bool,
    planted: bool,
}
impl Breakpoint {
    const fn empty() -> Self {
        Self {
            address: 0,
            original: [0; 4],
            active: false,
            planted: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Stop,
    Continue,
    Step,
}
#[derive(Clone, Copy)]
struct StepOver {
    cpu: usize,
    address: u64,
    report_step: bool,
    resume: [Action; CPU_COUNT],
    remaining_mask: u8,
}
enum StepOverResult {
    None,
    Hidden,
    Report,
}
struct GdbTarget {
    breakpoints: [Breakpoint; MAX_BREAKPOINTS],
    explicit: [Option<Action>; CPU_COUNT],
    scheduler_lock: bool,
    step_over: Option<StepOver>,
}
impl GdbTarget {
    const fn new() -> Self {
        Self {
            breakpoints: [Breakpoint::empty(); MAX_BREAKPOINTS],
            explicit: [None; CPU_COUNT],
            scheduler_lock: false,
            step_over: None,
        }
    }
    fn index(tid: Tid) -> Option<usize> {
        let cpu = tid.get().checked_sub(1)?;
        (cpu < CPU_COUNT && Self::stopped_mask() & (1 << cpu) != 0).then_some(cpu)
    }
    fn stopped_mask() -> u8 {
        debug::target_mask() & vcpu::guest_mask()
    }
    fn tid(cpu: usize) -> Tid {
        NonZeroUsize::new(cpu + 1).unwrap()
    }
    fn breakpoint(&self, address: u64) -> Option<usize> {
        self.breakpoints
            .iter()
            .position(|bp| bp.active && bp.address == address)
    }
    fn planted(&self) -> bool {
        self.breakpoints.iter().any(|bp| bp.active && bp.planted)
    }
    fn patch(address: u64, instruction: [u8; 4]) {
        unsafe {
            core::ptr::write_volatile(address as *mut u32, u32::from_le_bytes(instruction));
            asm!("dsb sy", "ic iallu", "dsb sy", "isb", options(nostack));
        }
    }
    fn plant(&mut self, index: usize) {
        if !self.breakpoints[index].planted {
            Self::patch(self.breakpoints[index].address, BRK.to_le_bytes());
            self.breakpoints[index].planted = true;
        }
    }
    fn unplant(&mut self, index: usize) {
        if self.breakpoints[index].planted {
            Self::patch(
                self.breakpoints[index].address,
                self.breakpoints[index].original,
            );
            self.breakpoints[index].planted = false;
        }
    }
    fn stop_reason(&self, cpu: usize) -> MultiThreadStopReason<u64> {
        let tid = Self::tid(cpu);
        match debug::stop_cause(cpu) {
            3 => MultiThreadStopReason::DoneStep,
            2 if debug::context(cpu).is_some_and(|ctx| self.breakpoint(ctx.pc).is_some()) => {
                MultiThreadStopReason::SwBreak(tid)
            }
            _ => MultiThreadStopReason::SignalWithThread {
                tid,
                signal: Signal::SIGTRAP,
            },
        }
    }
    fn apply_plan(&mut self, plan: [Action; CPU_COUNT]) -> Result<(), &'static str> {
        let bp = self.planted();
        let active = Self::stopped_mask();
        for (cpu, action) in plan.iter().enumerate() {
            if active & (1 << cpu) == 0 {
                continue;
            }
            if *action == Action::Stop {
                debug::set_resume(cpu, false);
            } else {
                if !debug::prepare_resume(cpu, *action == Action::Step, bp) {
                    return Err("missing stopped CPU context");
                }
                debug::set_resume(cpu, true);
            }
        }
        debug::resume_world();
        Ok(())
    }
    fn start_step_over(
        &mut self,
        plan: [Action; CPU_COUNT],
        cpu: usize,
        remaining_mask: u8,
        report_step: bool,
    ) -> Result<(), &'static str> {
        let address = debug::context(cpu).ok_or("missing stopped CPU context")?.pc;
        let index = self.breakpoint(address).ok_or("missing breakpoint")?;
        self.unplant(index);
        self.step_over = Some(StepOver {
            cpu,
            address,
            report_step,
            resume: plan,
            remaining_mask,
        });
        let mut one = [Action::Stop; CPU_COUNT];
        one[cpu] = Action::Step;
        self.apply_plan(one)
    }
    fn start_plan(&mut self, plan: [Action; CPU_COUNT]) -> Result<(), &'static str> {
        let mut mask = 0u8;
        for cpu in 0..CPU_COUNT {
            if plan[cpu] != Action::Stop
                && debug::context(cpu).is_some_and(|ctx| self.breakpoint(ctx.pc).is_some())
            {
                mask |= 1 << cpu;
            }
        }
        if let Some(cpu) =
            (0..CPU_COUNT).find(|&cpu| mask & (1 << cpu) != 0 && plan[cpu] == Action::Step)
        {
            // A requested step at a BRK is itself the visible stop. Other
            // CPUs remain parked until the next GDB resume command.
            self.start_step_over(plan, cpu, 0, true)
        } else if mask != 0 {
            let cpu = mask.trailing_zeros() as usize;
            self.start_step_over(plan, cpu, mask & !(1 << cpu), false)
        } else {
            self.apply_plan(plan)
        }
    }
    fn complete_step_over(&mut self, cpu: usize) -> Result<StepOverResult, &'static str> {
        let Some(pending) = self.step_over else {
            return Ok(StepOverResult::None);
        };
        if pending.cpu != cpu {
            return Ok(StepOverResult::None);
        }
        if let Some(index) = self.breakpoint(pending.address) {
            self.plant(index);
        }
        self.step_over = None;
        if debug::stop_cause(cpu) != 3 {
            return Ok(StepOverResult::None);
        }
        debug::clear_stop_cause(cpu);
        if pending.report_step {
            // A requested step is reported after the original instruction executes.
            debug::set_resume(cpu, false);
            Ok(StepOverResult::Report)
        } else if pending.remaining_mask != 0 {
            let next = pending.remaining_mask.trailing_zeros() as usize;
            self.start_step_over(
                pending.resume,
                next,
                pending.remaining_mask & !(1 << next),
                false,
            )?;
            Ok(StepOverResult::Hidden)
        } else {
            self.apply_plan(pending.resume)?;
            Ok(StepOverResult::Hidden)
        }
    }
    fn remove_all(&mut self) {
        for index in 0..MAX_BREAKPOINTS {
            if self.breakpoints[index].active {
                self.unplant(index);
            }
            self.breakpoints[index] = Breakpoint::empty();
        }
        self.step_over = None;
        BREAKPOINT_COUNT.store(0, Ordering::Release);
    }
}

struct GdbConnection;
impl Connection for GdbConnection {
    type Error = ();
    fn write(&mut self, byte: u8) -> Result<(), ()> {
        (usb::gdb_write(&[byte]) == 1).then_some(()).ok_or(())
    }
    fn flush(&mut self) -> Result<(), ()> {
        usb::gdb_flush();
        Ok(())
    }
}

impl Target for GdbTarget {
    type Arch = AArch64;
    type Error = &'static str;
    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::MultiThread(self)
    }
    fn support_breakpoints(
        &mut self,
    ) -> Option<gdbstub::target::ext::breakpoints::BreakpointsOps<'_, Self>> {
        Some(self)
    }
}
impl MultiThreadBase for GdbTarget {
    fn read_registers(&mut self, regs: &mut AArch64CoreRegs, tid: Tid) -> TargetResult<(), Self> {
        if !debug::world_stopped() {
            return Err(TargetError::NonFatal);
        }
        let cpu = Self::index(tid).ok_or(TargetError::NonFatal)?;
        let ctx = debug::context(cpu).ok_or(TargetError::NonFatal)?;
        regs.x = ctx.x;
        regs.sp = ctx.sp();
        regs.pc = ctx.pc;
        regs.cpsr = ctx.pstate as u32;
        regs.v = ctx.v;
        regs.fpcr = ctx.fpcr;
        regs.fpsr = ctx.fpsr;
        Ok(())
    }
    fn write_registers(&mut self, regs: &AArch64CoreRegs, tid: Tid) -> TargetResult<(), Self> {
        if !matches!(regs.cpsr & 0xf, 0 | 4 | 5) || regs.pc & 3 != 0 {
            return Err(TargetError::NonFatal);
        }
        let cpu = Self::index(tid).ok_or(TargetError::NonFatal)?;
        debug::edit_context(cpu, |ctx| {
            ctx.x = regs.x;
            ctx.pc = regs.pc;
            ctx.pstate = u64::from(regs.cpsr);
            ctx.set_sp(regs.sp);
            ctx.v = regs.v;
            ctx.fpcr = regs.fpcr;
            ctx.fpsr = regs.fpsr;
        })
        .ok_or(TargetError::NonFatal)
    }
    fn read_addrs(&mut self, start: u64, bytes: &mut [u8], _tid: Tid) -> TargetResult<usize, Self> {
        if !debug::world_stopped() || !debuggable_ram(start, bytes.len()) {
            return Err(TargetError::NonFatal);
        }
        for (offset, byte) in bytes.iter_mut().enumerate() {
            let address = start + offset as u64;
            *byte = self
                .breakpoints
                .iter()
                .find_map(|bp| {
                    (bp.active && (bp.address..bp.address + 4).contains(&address))
                        .then_some(bp.original[(address - bp.address) as usize])
                })
                .unwrap_or_else(|| unsafe { core::ptr::read_volatile(address as *const u8) });
        }
        Ok(bytes.len())
    }
    fn write_addrs(&mut self, start: u64, bytes: &[u8], _tid: Tid) -> TargetResult<(), Self> {
        if !debug::world_stopped() || !debuggable_ram(start, bytes.len()) {
            return Err(TargetError::NonFatal);
        }
        for (offset, byte) in bytes.iter().enumerate() {
            let address = start + offset as u64;
            if let Some(bp) = self
                .breakpoints
                .iter_mut()
                .find(|bp| bp.active && (bp.address..bp.address + 4).contains(&address))
            {
                bp.original[(address - bp.address) as usize] = *byte;
                if !bp.planted {
                    unsafe { core::ptr::write_volatile(address as *mut u8, *byte) };
                }
            } else {
                unsafe { core::ptr::write_volatile(address as *mut u8, *byte) };
            }
        }
        unsafe { asm!("dsb sy", "ic iallu", "dsb sy", "isb", options(nostack)) };
        Ok(())
    }
    fn list_active_threads(&mut self, callback: &mut dyn FnMut(Tid)) -> Result<(), Self::Error> {
        let active = Self::stopped_mask();
        for cpu in 0..CPU_COUNT {
            if active & (1 << cpu) != 0 {
                callback(Self::tid(cpu));
            }
        }
        Ok(())
    }
    fn support_resume(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::multithread::MultiThreadResumeOps<'_, Self>> {
        Some(self)
    }
}
impl MultiThreadResume for GdbTarget {
    fn clear_resume_actions(&mut self) -> Result<(), Self::Error> {
        self.explicit = [None; CPU_COUNT];
        self.scheduler_lock = false;
        Ok(())
    }
    fn set_resume_action_continue(
        &mut self,
        tid: Tid,
        signal: Option<Signal>,
    ) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("signal delivery unsupported");
        }
        self.explicit[Self::index(tid).ok_or("invalid CPU")?] = Some(Action::Continue);
        Ok(())
    }
    fn resume(&mut self) -> Result<(), Self::Error> {
        let mut plan = [Action::Stop; CPU_COUNT];
        for (cpu, action) in plan.iter_mut().enumerate() {
            *action = self.explicit[cpu].unwrap_or(if self.scheduler_lock {
                Action::Stop
            } else {
                Action::Continue
            });
        }
        self.start_plan(plan)
    }
    fn support_single_step(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::multithread::MultiThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
    fn support_scheduler_locking(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::multithread::MultiThreadSchedulerLockingOps<'_, Self>>
    {
        Some(self)
    }
}
impl MultiThreadSingleStep for GdbTarget {
    fn set_resume_action_step(
        &mut self,
        tid: Tid,
        signal: Option<Signal>,
    ) -> Result<(), Self::Error> {
        if signal.is_some() {
            return Err("signal delivery unsupported");
        }
        self.explicit[Self::index(tid).ok_or("invalid CPU")?] = Some(Action::Step);
        Ok(())
    }
}
impl MultiThreadSchedulerLocking for GdbTarget {
    fn set_resume_action_scheduler_lock(&mut self) -> Result<(), Self::Error> {
        self.scheduler_lock = true;
        Ok(())
    }
}
impl Breakpoints for GdbTarget {
    fn support_sw_breakpoint(
        &mut self,
    ) -> Option<gdbstub::target::ext::breakpoints::SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}
impl SwBreakpoint for GdbTarget {
    fn add_sw_breakpoint(&mut self, address: u64, kind: usize) -> TargetResult<bool, Self> {
        if !debug::world_stopped() || kind != 4 || address & 3 != 0 || !debuggable_ram(address, 4) {
            return Ok(false);
        }
        if self.breakpoint(address).is_some() {
            return Ok(true);
        }
        let Some(index) = self.breakpoints.iter().position(|bp| !bp.active) else {
            return Ok(false);
        };
        let original = unsafe { core::ptr::read_volatile(address as *const u32) }.to_le_bytes();
        self.breakpoints[index] = Breakpoint {
            address,
            original,
            active: true,
            planted: false,
        };
        self.plant(index);
        BREAKPOINT_COUNT.store(
            BREAKPOINT_COUNT.load(Ordering::Acquire) + 1,
            Ordering::Release,
        );
        Ok(true)
    }
    fn remove_sw_breakpoint(&mut self, address: u64, kind: usize) -> TargetResult<bool, Self> {
        if !debug::world_stopped() || kind != 4 {
            return Ok(false);
        }
        let Some(index) = self.breakpoint(address) else {
            return Ok(false);
        };
        self.unplant(index);
        self.breakpoints[index] = Breakpoint::empty();
        BREAKPOINT_COUNT.store(
            BREAKPOINT_COUNT.load(Ordering::Acquire) - 1,
            Ordering::Release,
        );
        Ok(true)
    }
}

fn debuggable_ram(address: u64, size: usize) -> bool {
    address >= GUEST_RAM_BASE
        && address
            .checked_add(size as u64)
            .is_some_and(|end| end <= GUEST_RAM_END)
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}
pub fn breakpoint_count() -> usize {
    BREAKPOINT_COUNT.load(Ordering::Acquire)
}

pub fn on_stop_timeout() {
    // A live BRK may still be in RAM. Keep TDE and the RSP session resident:
    // clearing either before every CPU has stopped would expose BRK to EL1.
    if !active() {
        let _ = debug::disconnect_requested();
        let machine = unsafe { (&mut *MACHINE.0.get()).take() };
        drop(machine);
    }
    for cpu in 0..CPU_COUNT {
        debug::clear_stop_cause(cpu);
    }
}

fn save_machine(machine: Machine) {
    unsafe { *MACHINE.0.get() = Some(machine) };
}
fn cleanup(target: &mut GdbTarget, machine: Option<Machine>) -> ! {
    drop(machine);
    let _ = debug::disconnect_requested();
    debug::cancel_attach();
    let stopped = debug::world_stopped() || debug::stop_world();
    if stopped {
        target.remove_all();
        NEEDS_CLEANUP.store(false, Ordering::Release);
    } else {
        NEEDS_CLEANUP.store(breakpoint_count() != 0, Ordering::Release);
    }
    unsafe { *MACHINE.0.get() = None };
    // If the stop timed out, retain debug ownership until a later stop can
    // remove every BRK. This also covers a detached USB cable.
    ACTIVE.store(!stopped, Ordering::Release);
    for cpu in 0..CPU_COUNT {
        if debug::context(cpu).is_some() {
            if stopped {
                let _ = debug::prepare_resume(cpu, false, false);
            }
            debug::set_resume(cpu, true);
            debug::clear_stop_cause(cpu);
        }
    }
    if stopped {
        debug::resume_world();
    }
    usb::gdb_flush();
    debug::return_to_guest(0)
}

pub fn monitor() -> ! {
    let target = unsafe { &mut *TARGET.0.get() };
    if NEEDS_CLEANUP.load(Ordering::Acquire) && debug::world_stopped() {
        target.remove_all();
        NEEDS_CLEANUP.store(false, Ordering::Release);
    }
    let mut machine = match unsafe { (&mut *MACHINE.0.get()).take() } {
        Some(machine) => machine,
        None => {
            let packet = unsafe { &mut *PACKET.0.get() };
            let stub = GdbStub::<GdbTarget, GdbConnection>::builder(GdbConnection)
                .with_packet_buffer(packet)
                .build()
                .unwrap();
            match stub.run_state_machine(target) {
                Ok(machine) => {
                    ACTIVE.store(true, Ordering::Release);
                    debug::cancel_attach();
                    machine
                }
                Err(_) => cleanup(target, None),
            }
        }
    };
    loop {
        usb::service();
        if !usb::gdb_connected() || debug::disconnect_requested() {
            cleanup(target, Some(machine));
        }
        if let Some(cpu) = debug::pending_stop_cpu() {
            if !debug::world_stopped() && !debug::stop_world() {
                cleanup(target, Some(machine));
            }
            let step_over = match target.complete_step_over(cpu) {
                Ok(result) => result,
                Err(_) => cleanup(target, Some(machine)),
            };
            if matches!(step_over, StepOverResult::Hidden) {
                // The RSP command remains in the Running state; continue the
                // original resume plan without exposing the internal step.
            } else if let GdbStubStateMachine::Running(running) = machine {
                let reason = if matches!(step_over, StepOverResult::Report) {
                    MultiThreadStopReason::DoneStep
                } else {
                    target.stop_reason(cpu)
                };
                debug::clear_stop_cause(cpu);
                machine = match running.report_stop(target, reason) {
                    Ok(machine) => machine,
                    Err(_) => cleanup(target, None),
                };
                continue;
            }
        }
        let mut bytes = [0; 256];
        let count = usb::gdb_read(&mut bytes);
        for byte in &bytes[..count] {
            machine = match machine {
                GdbStubStateMachine::Idle(idle) => match idle.incoming_data(target, *byte) {
                    Ok(machine) => machine,
                    Err(_) => cleanup(target, None),
                },
                GdbStubStateMachine::Running(running) => match running.incoming_data(target, *byte)
                {
                    Ok(machine) => machine,
                    Err(_) => cleanup(target, None),
                },
                GdbStubStateMachine::CtrlCInterrupt(ctrlc) => {
                    if !debug::world_stopped() && !debug::stop_world() {
                        cleanup(target, None);
                    }
                    match ctrlc.interrupt_handled(
                        target,
                        Some(MultiThreadStopReason::SignalWithThread {
                            tid: GdbTarget::tid(0),
                            signal: Signal::SIGINT,
                        }),
                    ) {
                        Ok(machine) => machine,
                        Err(_) => cleanup(target, None),
                    }
                }
                GdbStubStateMachine::Disconnected(disconnected) => cleanup(
                    target,
                    Some(GdbStubStateMachine::Disconnected(disconnected)),
                ),
            };
        }
        machine = match machine {
            GdbStubStateMachine::CtrlCInterrupt(ctrlc) => {
                if !debug::world_stopped() && !debug::stop_world() {
                    cleanup(target, None);
                }
                match ctrlc.interrupt_handled(
                    target,
                    Some(MultiThreadStopReason::SignalWithThread {
                        tid: GdbTarget::tid(0),
                        signal: Signal::SIGINT,
                    }),
                ) {
                    Ok(machine) => machine,
                    Err(_) => cleanup(target, None),
                }
            }
            GdbStubStateMachine::Disconnected(disconnected) => cleanup(
                target,
                Some(GdbStubStateMachine::Disconnected(disconnected)),
            ),
            other => other,
        };
        if matches!(machine, GdbStubStateMachine::Running(_)) && !debug::world_stopped() {
            if debug::parked_mask() & 1 == 0 {
                cleanup(target, Some(machine));
            }
            if debug::target_mask() & 1 != 0
                && debug::context(0).is_some()
                && debug::resume_requested(0)
            {
                usb::gdb_flush();
                save_machine(machine);
                debug::return_to_guest(0);
            }
        }
        core::hint::spin_loop();
    }
}
