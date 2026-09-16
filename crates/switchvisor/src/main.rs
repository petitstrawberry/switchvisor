//! EL2 monitor with protected resident RAM and an external raw BL33.
#![no_std]
#![no_main]

use arch::aarch64::{self as cpu, mmu, park, stage2};
use core::{arch::asm, fmt::Write, panic::PanicInfo};
use platform::tegra210::framebuffer::{self, Screen};
use switchvisor::payload::{
    CONFIG_SIZE, EXIT_HVC, LOAD_BASE, Payload, RESIDENT_BASE, RESIDENT_SIZE, STACK_TOP,
};
use vm::vcpu;

mod arch;
mod debug;
mod gdb;
mod platform;
mod usb;
mod vm;

#[used]
#[unsafe(no_mangle)]
static _guest_hcr: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(switchvisor::stage2::HCR);

// The packager patches this reserved slot. Volatile reads prevent constant folding its zeros.
#[used]
#[unsafe(link_section = ".launch_config")]
static LAUNCH_CONFIG: [u8; CONFIG_SIZE] = [0; CONFIG_SIZE];

unsafe extern "C" {
    static _boot_handoff: [u64; 8];
    static _start: u8;
    static __image_end: u8;
    static __bss_end: u8;
}

fn console() -> Screen {
    framebuffer::console().unwrap_or_else(|_| park())
}

#[unsafe(no_mangle)]
extern "C" fn rust_boot() -> ! {
    let handoff = unsafe { core::ptr::addr_of!(_boot_handoff).read_volatile() };
    let sctlr: u64;
    let current_el: u64;
    unsafe {
        asm!("mrs {value}, sctlr_el2", value = out(reg) sctlr, options(nomem, nostack));
        asm!("mrs {value}, CurrentEL", value = out(reg) current_el, options(nomem, nostack));
    }
    let mut screen = console();
    screen.clear();
    let _ = writeln!(screen, "SWITCHVISOR\n");
    let _ = writeln!(screen, "CURRENTEL = EL{}", current_el >> 2);
    let _ = writeln!(screen, "SCTLR_EL2 = {sctlr:016x}");
    let _ = writeln!(screen, "FRAMEBUFFER = F5A00000 BGRA ROTATION=3\n");
    for (index, value) in handoff.iter().enumerate() {
        let _ = writeln!(screen, "X{index} = {value:016x}");
    }
    let descriptor = unsafe { core::ptr::addr_of!(LAUNCH_CONFIG).read_volatile() };
    match Payload::decode(&descriptor) {
        Ok(Some(payload)) => launch_payload(payload, &handoff, &mut screen),
        Ok(None) => (),
        Err(error) => {
            let _ = writeln!(screen, "\nPAYLOAD REJECTED: {error}");
            park()
        }
    }
    let _ = writeln!(screen, "\nPAYLOAD DESCRIPTOR MISSING\nCPU0 PARKED");
    unsafe {
        asm!("dsb sy", options(nostack));
    }
    park()
}

fn launch_payload(payload: Payload, handoff: &[u64; 8], screen: &mut Screen) -> ! {
    let resident = core::ptr::addr_of!(_start) as u64;
    let image_end = core::ptr::addr_of!(__image_end) as u64;
    if resident != RESIDENT_BASE
        || image_end - resident != payload.bootstrap_size
        || core::ptr::addr_of!(__bss_end) as u64 > RESIDENT_BASE + RESIDENT_SIZE
    {
        let _ = writeln!(screen, "\nPAYLOAD REJECTED: BOOTSTRAP PLACEMENT");
        park()
    }
    // All source validation completes before the package at AA000000 is overwritten.
    // Keep no Rust references into the package across the overlapping raw copy.
    let source = {
        let package = unsafe {
            core::slice::from_raw_parts(LOAD_BASE as *const u8, payload.package_size as usize)
        };
        match payload.source(package) {
            Ok(source) => source.as_ptr(),
            Err(error) => {
                let _ = writeln!(screen, "\nPAYLOAD REJECTED: {error}");
                park()
            }
        }
    };
    let mut entry = match payload.entry() {
        Ok(entry) => entry,
        Err(_) => park(),
    };
    let mut registers = if payload.preserve_boot_args {
        *handoff
    } else {
        payload.registers
    };
    let _ = writeln!(
        screen,
        "\nEXTERNAL RAW PAYLOAD\nLOAD = {LOAD_BASE:016x}\nENTRY = {entry:016x}\nFILE = {:x} RUNTIME = {:x}\nHANDOFF TO EL1",
        payload.file_size, payload.runtime_size
    );
    unsafe {
        // The fixed payload/source windows lie below resident EL2. ptr::copy handles overlap.
        core::ptr::copy(source, LOAD_BASE as *mut u8, payload.file_size as usize);
        core::ptr::write_bytes(
            (LOAD_BASE + payload.file_size) as *mut u8,
            0,
            (payload.runtime_size - payload.file_size) as usize,
        );
        core::ptr::write_bytes((STACK_TOP - 64 * 1024) as *mut u8, 0, 64 * 1024);
    }
    let gic = cpu::interrupt::detect_layout();
    cpu::interrupt::select_layout(gic);
    let physical_usb = payload.usb_uart || payload.usb_control || payload.usb_gdb;
    if stage2::prepare(physical_usb, gic).is_err() || mmu::prepare().is_err() {
        let _ = writeln!(screen, "STAGE2 REJECTED: TABLE PLACEMENT");
        park()
    }
    unsafe {
        mmu::enable();
        stage2::enable();
    }
    vcpu::initialize();
    match usb::initialize(
        physical_usb,
        payload.usb_uart,
        payload.require_upload,
        payload.usb_gdb,
        entry,
    ) {
        Ok(true) => {
            let _ = writeln!(screen, "USB COMPOSITE ON - WAITING FOR HOST");
        }
        Ok(false) if physical_usb => {
            let _ = writeln!(screen, "USB UNAVAILABLE - NO TEGRA210 IP");
        }
        Err(error) => {
            let _ = writeln!(screen, "USB INIT FAILED: {error:?}");
        }
        _ => (),
    }
    vm::interrupt::initialize(
        gic,
        usb::available().then_some(switchvisor::drivers::usb::tegra210::INTERRUPT_ID),
        payload.usb_gdb && usb::available(),
    );
    cpu::interrupt::select_usb_interrupt(usb::available());
    debug::initialize(payload.usb_gdb && usb::available());
    cpu::interrupt::select_debug_sgi(debug::enabled());
    if let Err(error) = cpu::interrupt::initialize() {
        let _ = writeln!(screen, "VGIC INIT FAILED: {error:?}");
        park()
    }
    _guest_hcr.store(
        switchvisor::stage2::HCR,
        core::sync::atomic::Ordering::Release,
    );
    if let Some(uploaded) = usb::preboot(screen) {
        entry = uploaded.entry;
        registers = if uploaded.preserve_boot_args() {
            *handoff
        } else {
            uploaded.registers
        };
        let _ = writeln!(
            screen,
            "USB GUEST BUNDLE READY\nENTRY = {entry:016x}\nIMAGES = {}",
            uploaded.image_count
        );
    }
    usb::enter_guest(entry);
    if debug::attach_requested() {
        cpu::interrupt::send_debug_kick(1);
    }
    unsafe {
        asm!("dsb sy", "ic iallu", "dsb sy", "isb", options(nostack));
    }
    let _ = writeln!(screen, "STAGE2 ON - VMM RAM EXCLUDED");
    vcpu::record(vcpu::Stage::Guest);
    unsafe { cpu::enter_payload(registers.as_ptr(), entry, STACK_TOP) }
}

fn guest_instruction(virtual_address: u64) -> Option<u32> {
    if virtual_address & 3 != 0 {
        return None;
    }
    let saved_par: u64;
    let translated: u64;
    unsafe {
        asm!("mrs {saved}, par_el1", saved = out(reg) saved_par, options(nomem, nostack));
        asm!(
            "at s12e1r, {address}",
            "isb",
            "mrs {translated}, par_el1",
            address = in(reg) virtual_address,
            translated = out(reg) translated,
            options(nostack),
        );
        asm!("msr par_el1, {saved}", saved = in(reg) saved_par, options(nomem, nostack));
    }
    if translated & 1 != 0 {
        return None;
    }
    let physical_address = (translated & 0x0000_ffff_ffff_f000) | (virtual_address & 0xfff);
    if physical_address >= switchvisor::IPA_LIMIT
        || (RESIDENT_BASE..RESIDENT_BASE + RESIDENT_SIZE).contains(&physical_address)
    {
        return None;
    }
    Some(unsafe { core::ptr::read_volatile(physical_address as *const u32) })
}

#[unsafe(no_mangle)]
extern "C" fn rust_exception(registers: &mut [u64; 31]) -> u64 {
    let esr: u64;
    let far: u64;
    let hpfar: u64;
    let elr: u64;
    let spsr: u64;
    unsafe {
        asm!("mrs {value}, esr_el2", value = out(reg) esr, options(nomem, nostack));
        asm!("mrs {value}, far_el2", value = out(reg) far, options(nomem, nostack));
        asm!("mrs {value}, hpfar_el2", value = out(reg) hpfar, options(nomem, nostack));
        asm!("mrs {value}, elr_el2", value = out(reg) elr, options(nomem, nostack));
        asm!("mrs {value}, spsr_el2", value = out(reg) spsr, options(nomem, nostack));
    }
    if let Some(stop) = debug::sysreg_trap(esr, registers, elr, spsr) {
        return u64::from(stop);
    }
    if matches!(spsr & 0xf, 0 | 4 | 5) && debug::exception(esr, far) {
        return 1;
    }
    if esr >> 26 == 0x17 && esr & 0xffff == 0 && spsr & 0xf == 5 {
        if !vcpu::handle(registers) {
            // Other SMCCC calls retain the native EL3 firmware service.
            unsafe { cpu::forward_smc(registers.as_mut_ptr()) };
        }
        usb::service();
        vcpu::record(vcpu::Stage::Guest);
        unsafe {
            asm!("msr elr_el2, {value}", "msr spsr_el2, {spsr}",
                value = in(reg) (elr + 4), spsr = in(reg) spsr, options(nostack));
        }
        return u64::from(debug::emulated_step() || debug::irq_should_stop());
    }
    if spsr & 0xf == 5
        && (vm::mmio::emulate(esr, far, hpfar, registers)
            || guest_instruction(elr).is_some_and(|instruction| {
                vm::interrupt::emulate_store_post_index(esr, far, hpfar, instruction, registers)
            }))
    {
        cpu::interrupt::synchronize_distributor();
        usb::service();
        unsafe {
            asm!("msr elr_el2, {value}", "msr spsr_el2, {spsr}",
                value = in(reg) (elr + 4), spsr = in(reg) spsr, options(nostack));
        }
        return u64::from(debug::emulated_step() || debug::irq_should_stop());
    }
    let mut screen = console();
    screen.clear();
    if esr >> 26 == 0x16 && esr & 0xffff == u64::from(EXIT_HVC) && spsr & 0xf == 5 {
        let _ = writeln!(
            screen,
            "PAYLOAD EXIT\nCODE = {:016x}\nEL1 PAYLOAD RETURNED\nCPU0 PARKED",
            registers[0]
        );
        unsafe {
            asm!("dsb sy", options(nostack));
        }
        park()
    }
    let _ = writeln!(
        screen,
        "FATAL EL2 EXCEPTION\nESR = {esr:016x}\nFAR = {far:016x}\nHPFAR = {hpfar:016x}\nELR = {elr:016x}\nSPSR = {spsr:016x}\nCPU PARKED"
    );
    vcpu::diagnostics(&mut screen);
    debug::diagnostics(&mut screen);
    if esr >> 26 == 0x2f {
        let _ = writeln!(screen, "SERROR: FAR AND ELR MAY BE UNRELATED");
    }
    unsafe {
        asm!("dsb sy", options(nostack));
    }
    park()
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    let mut screen = console();
    screen.clear();
    let _ = writeln!(screen, "RUST PANIC\n{info}");
    debug::diagnostics(&mut screen);
    unsafe {
        asm!("dsb sy", options(nostack));
    }
    park()
}
