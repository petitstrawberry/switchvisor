//! Own the virtual UART and its physical transport, including lock and IRQ policy.
use crate::{
    arch::aarch64::sync::Mutex,
    platform::tegra210::io::{Hardware, UsbDma},
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use switchvisor::{
    drivers::{
        Clock, Driver, Mmio, RxTransport, TxTransport,
        usb::tegra210::{Error, Xudc},
    },
    vdev::{self, uart::Uart},
};

static UART: Mutex<Uart> = Mutex::new(Uart::new());

struct Transport {
    driver: Xudc<Hardware, UsbDma>,
    error: Option<Error>,
}
static USB: Mutex<Transport> = Mutex::new(Transport {
    driver: Xudc::new(Hardware, UsbDma),
    error: None,
});
static ENABLED: AtomicBool = AtomicBool::new(false);
static AVAILABLE: AtomicBool = AtomicBool::new(false);
static IRQ_ASSERTED: AtomicBool = AtomicBool::new(false);
const POLL_INTERVAL_US: u64 = 250;
static LAST_SERVICE: AtomicU64 = AtomicU64::new(0);

pub fn initialize(enabled: bool) -> Result<bool, Error> {
    ENABLED.store(enabled, Ordering::Release);
    AVAILABLE.store(false, Ordering::Release);
    IRQ_ASSERTED.store(false, Ordering::Release);
    if !enabled {
        return Ok(false);
    }
    // QEMU CPU tests have no Tegra USB IP. Never touch absent/unpowered XUDC.
    if (Hardware.read32(0x7000_0804) >> 8) & 0xff != 0x21 {
        return Ok(false);
    }
    unsafe {
        USB.with(|state| state.driver.initialize())?;
    }
    AVAILABLE.store(true, Ordering::Release);
    Ok(true)
}

pub fn service() {
    if !AVAILABLE.load(Ordering::Acquire) {
        return;
    }
    // UART register accesses and other trapped guest exits can happen much
    // faster than USB progresses. Skip polling and its mutex between slots.
    let last = LAST_SERVICE.load(Ordering::Acquire);
    if Hardware.now_us().wrapping_sub(last) < POLL_INTERVAL_US {
        return;
    }
    let asserted = unsafe {
        USB.with(|state| {
            // Another vCPU may have serviced USB while this one waited for it.
            let now = Hardware.now_us();
            if now.wrapping_sub(LAST_SERVICE.load(Ordering::Acquire)) < POLL_INTERVAL_US {
                return IRQ_ASSERTED.load(Ordering::Acquire);
            }
            LAST_SERVICE.store(now, Ordering::Release);
            if let Err(error) = service_transport(&mut state.driver) {
                state.error = Some(error);
            }
            observe_interrupt()
        })
    };
    if asserted {
        crate::arch::aarch64::interrupt::pend_console_interrupt();
    }
}

/// Service a physical XUDC event immediately and return the virtual UART line.
pub fn service_interrupt() -> bool {
    if !AVAILABLE.load(Ordering::Acquire) {
        return false;
    }
    unsafe {
        USB.with(|state| {
            LAST_SERVICE.store(Hardware.now_us(), Ordering::Release);
            if let Err(error) = service_transport(&mut state.driver) {
                state.error = Some(error);
            }
            observe_interrupt()
        })
    }
}

/// A bounded boot-time probe while Hekate's framebuffer is still available.
/// Guest execution proceeds after two seconds even with no cable or host.
pub fn probe(screen: &mut impl core::fmt::Write) {
    if !AVAILABLE.load(Ordering::Acquire) {
        return;
    }
    let _ = writeln!(screen, "USB ENUMERATION PROBE (2S)");
    let start = Hardware.now_us();
    while Hardware.now_us().wrapping_sub(start) < 2_000_000 {
        service();
        Hardware.delay_us(100);
    }
    unsafe {
        USB.with(|state| {
            let status = state.driver.snapshot();
            let stats = &state.driver.statistics;
            let _ = writeln!(
                screen,
                "USB PORT={:08x} VBUSID={:08x} HALT={:08x}",
                status.port, status.vbus_id, status.port_halt
            );
            let _ = writeln!(
                screen,
                "USB CTRL={:08x} EPHALT={:08x} PAUSE={:08x} EP0={} CFG={} DTR={}",
                status.control,
                status.endpoint_halt,
                status.endpoint_pause,
                status.ep0_state,
                status.configuration,
                u8::from(status.dtr)
            );
            let _ = writeln!(
                screen,
                "USB ENQ={:08x} DEQ={:08x} EVENTS={} SETUPS={}",
                status.enqueue, status.dequeue, stats.events, stats.setups
            );
            let _ = writeln!(
                screen,
                "USB ERR={:?} LAST={:08x} {:08x} {:08x} {:08x}",
                state.error,
                stats.last_event[0],
                stats.last_event[1],
                stats.last_event[2],
                stats.last_event[3]
            );
        });
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn available() -> bool {
    AVAILABLE.load(Ordering::Acquire)
}

/// Serialize shared USB clock/power MMIO with driver polling and other vCPUs.
/// Callers must satisfy the masked, non-reentrant EL2 mutex preconditions.
pub(super) unsafe fn with_io<R>(operation: impl FnOnce(&mut Hardware) -> R) -> R {
    unsafe { USB.with(|_| operation(&mut Hardware)) }
}

pub fn emulate_uart(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    // Only masked EL2 entry/exception paths access the virtual device.
    let (handled, asserted) = unsafe {
        UART.with(|uart| {
            let handled = vdev::emulate(uart, esr, far, hpfar, registers);
            let asserted = uart.interrupt_pending();
            IRQ_ASSERTED.store(asserted, Ordering::Release);
            (handled, asserted)
        })
    };
    if handled && asserted {
        crate::arch::aarch64::interrupt::pend_console_interrupt();
    }
    handled
}

fn service_transport<T: RxTransport + TxTransport>(transport: &mut T) -> Result<(), T::Error> {
    transport.poll()?;
    receive(transport)?;
    transmit(transport)
}

fn observe_interrupt() -> bool {
    unsafe {
        UART.with(|uart| {
            let asserted = uart.interrupt_pending();
            IRQ_ASSERTED.store(asserted, Ordering::Release);
            asserted
        })
    }
}

/// Transport -> UART lock order. Completed OUT data stays in driver DMA until copied.
fn receive<T: RxTransport>(transport: &mut T) -> Result<(), T::Error> {
    unsafe {
        UART.with(|uart| {
            let mut bytes = [0; 512];
            let capacity = uart.receive_capacity().min(bytes.len());
            if capacity == 0 {
                return Ok(());
            }
            let received = transport.receive(&mut bytes[..capacity])?;
            assert!(
                received <= capacity,
                "transport returned more bytes than requested"
            );
            assert_eq!(uart.receive(&bytes[..received]), received);
            Ok(())
        })
    }
}

/// Transport -> UART lock order. MMIO releases UART before servicing transport.
fn transmit<T: TxTransport>(transport: &mut T) -> Result<(), T::Error> {
    let capacity = transport.send_capacity().min(512);
    if capacity == 0 {
        return Ok(());
    }
    unsafe {
        UART.with(|uart| {
            let mut bytes = [0; 512];
            let length = uart.tx().peek(&mut bytes[..capacity]);
            let accepted = transport.send(&bytes[..length])?;
            assert!(
                accepted <= length,
                "transport accepted more bytes than offered"
            );
            uart.tx().consume(accepted);
            Ok(())
        })
    }
}
