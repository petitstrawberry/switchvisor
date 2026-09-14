//! Own the virtual UART and its physical transport, including lock order and polling.
use crate::{
    arch::aarch64::sync::Mutex,
    platform::tegra210::io::{Hardware, UsbDma},
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use switchvisor::{
    drivers::{
        Clock, Driver, Mmio, TxTransport,
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
const POLL_INTERVAL_US: u64 = 250;
static LAST_SERVICE: AtomicU64 = AtomicU64::new(0);

pub fn initialize(enabled: bool) -> Result<bool, Error> {
    ENABLED.store(enabled, Ordering::Release);
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
    // UART register accesses and trapped WFI can exit much faster than USB
    // progresses. Skip both driver polling and its mutex between service slots.
    let last = LAST_SERVICE.load(Ordering::Acquire);
    if Hardware.now_us().wrapping_sub(last) < POLL_INTERVAL_US {
        return;
    }
    unsafe {
        USB.with(|state| {
            // Another vCPU may have serviced USB while this one waited for it.
            let now = Hardware.now_us();
            if now.wrapping_sub(LAST_SERVICE.load(Ordering::Acquire)) < POLL_INTERVAL_US {
                return;
            }
            LAST_SERVICE.store(now, Ordering::Release);
            if let Err(error) = state
                .driver
                .poll()
                .and_then(|()| transmit(&mut state.driver))
            {
                state.error = Some(error);
            }
        });
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

/// Serialize shared USB clock/power MMIO with driver polling and other vCPUs.
/// Callers must satisfy the masked, non-reentrant EL2 mutex preconditions.
pub(super) unsafe fn with_io<R>(operation: impl FnOnce(&mut Hardware) -> R) -> R {
    unsafe { USB.with(|_| operation(&mut Hardware)) }
}

pub fn emulate_uart(esr: u64, far: u64, hpfar: u64, registers: &mut [u64; 31]) -> bool {
    // Only masked EL2 entry/exception paths access the virtual device.
    unsafe { UART.with(|uart| vdev::emulate(uart, esr, far, hpfar, registers)) }
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
