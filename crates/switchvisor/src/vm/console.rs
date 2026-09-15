//! Guest-visible virtual UART state, independent from its physical transport.

use crate::arch::aarch64::sync::Mutex;
use core::sync::atomic::{AtomicBool, Ordering};
use switchvisor::vdev::{self, uart::Uart};

static UART: Mutex<Uart> = Mutex::new(Uart::new());
static ENABLED: AtomicBool = AtomicBool::new(false);
static AVAILABLE: AtomicBool = AtomicBool::new(false);

pub fn configure(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
    AVAILABLE.store(false, Ordering::Release);
}

pub fn set_transport_available(available: bool) {
    AVAILABLE.store(
        ENABLED.load(Ordering::Acquire) && available,
        Ordering::Release,
    );
}

pub fn available() -> bool {
    AVAILABLE.load(Ordering::Acquire)
}

pub fn emulate_uart(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    if !ENABLED.load(Ordering::Acquire) {
        return false;
    }
    let (handled, asserted) = unsafe {
        UART.with(|uart| {
            let handled = vdev::emulate(uart, esr, far, hpfar, registers);
            let asserted = uart.interrupt_pending();
            (handled, asserted)
        })
    };
    if handled && asserted {
        crate::arch::aarch64::interrupt::pend_console_interrupt();
    }
    handled
}

pub fn receive_capacity() -> usize {
    if !available() {
        return 0;
    }
    unsafe { UART.with(|uart| uart.receive_capacity()) }
}

pub fn receive(bytes: &[u8]) -> usize {
    if !available() {
        return 0;
    }
    unsafe { UART.with(|uart| uart.receive(bytes)) }
}

pub fn peek_tx(output: &mut [u8]) -> usize {
    if !available() {
        return 0;
    }
    unsafe { UART.with(|uart| uart.tx().peek(output)) }
}

pub fn consume_tx(length: usize) {
    if available() {
        unsafe { UART.with(|uart| uart.tx().consume(length)) }
    }
}

pub fn interrupt_pending() -> bool {
    if !available() {
        return false;
    }
    unsafe { UART.with(|uart| uart.interrupt_pending()) }
}
