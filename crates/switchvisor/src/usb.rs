//! EL2 ownership and dispatch for the physical USB composite device.

use crate::{
    arch::aarch64::{park, sync::Mutex},
    platform::tegra210::io::{Hardware, UsbDma},
    vm::{console as guest_console, vcpu},
};
use core::{
    arch::asm,
    fmt::{self, Write},
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use switchvisor::{
    control::{Command, ParseError, Parser},
    drivers::{
        Clock, Driver, Mmio,
        usb::tegra210::{Channel, Error, PMC, Xudc},
    },
    loader::{
        Action, BundleDescriptor, Loader, MAX_MESSAGE_SIZE, MAX_RESPONSE_SIZE,
        State as LoaderState, Storage, StorageError, guest_range,
    },
};

const POLL_INTERVAL_US: u64 = 250;
const PREBOOT_WINDOW_US: u64 = 2_000_000;
const RESET_NONE: u8 = 0;
const RESET_NORMAL: u8 = 1;
const RESET_RCM: u8 = 2;

struct Fifo<const N: usize> {
    bytes: [u8; N],
    head: usize,
    len: usize,
}

impl<const N: usize> Fifo<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            head: 0,
            len: 0,
        }
    }
    fn capacity(&self) -> usize {
        N - self.len
    }
    fn push(&mut self, bytes: &[u8]) -> usize {
        let count = bytes.len().min(self.capacity());
        for (i, byte) in bytes[..count].iter().enumerate() {
            self.bytes[(self.head + self.len + i) % N] = *byte;
        }
        self.len += count;
        count
    }
    fn pop(&mut self, bytes: &mut [u8]) -> usize {
        let count = bytes.len().min(self.len);
        for (i, byte) in bytes[..count].iter_mut().enumerate() {
            *byte = self.bytes[(self.head + i) % N];
        }
        self.head = (self.head + count) % N;
        self.len -= count;
        count
    }
    fn peek(&self, bytes: &mut [u8]) -> usize {
        let count = bytes.len().min(self.len);
        for (i, byte) in bytes[..count].iter_mut().enumerate() {
            *byte = self.bytes[(self.head + i) % N];
        }
        count
    }
}

struct Output<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> Output<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            length: 0,
        }
    }

    fn pending(&self) -> &[u8] {
        &self.bytes[..self.length]
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), ()> {
        let end = self.length.checked_add(bytes.len()).ok_or(())?;
        let output = self.bytes.get_mut(self.length..end).ok_or(())?;
        output.copy_from_slice(bytes);
        self.length = end;
        Ok(())
    }

    fn consume(&mut self, length: usize) {
        assert!(length <= self.length);
        self.bytes.copy_within(length..self.length, 0);
        self.length -= length;
    }
}

impl<const N: usize> Write for Output<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.append(text.as_bytes()).map_err(|_| fmt::Error)
    }
}

struct Service {
    driver: Xudc<Hardware, UsbDma>,
    control: Parser,
    control_output: Output<512>,
    loader: Loader,
    loader_input: [u8; MAX_MESSAGE_SIZE],
    loader_output: Output<MAX_RESPONSE_SIZE>,
    gdb_rx: Fifo<4096>,
    gdb_tx: Fifo<16384>,
    gdb_reset_count: u64,
    boot: Option<BundleDescriptor>,
    error: Option<Error>,
}

static USB: Mutex<Service> = Mutex::new(Service {
    driver: Xudc::new(Hardware, UsbDma),
    control: Parser::new(),
    control_output: Output::new(),
    loader: Loader::new(),
    loader_input: [0; MAX_MESSAGE_SIZE],
    loader_output: Output::new(),
    gdb_rx: Fifo::new(),
    gdb_tx: Fifo::new(),
    gdb_reset_count: 0,
    boot: None,
    error: None,
});
static ENABLED: AtomicBool = AtomicBool::new(false);
static AVAILABLE: AtomicBool = AtomicBool::new(false);
static REQUIRE_UPLOAD: AtomicBool = AtomicBool::new(false);
static GDB_ENABLED: AtomicBool = AtomicBool::new(false);
static GUEST_RUNNING: AtomicBool = AtomicBool::new(false);
static LAST_SERVICE: AtomicU64 = AtomicU64::new(0);
static GUEST_ENTRY: AtomicU64 = AtomicU64::new(0);
static RESET: AtomicU8 = AtomicU8::new(RESET_NONE);

struct GuestMemory;

impl Storage for GuestMemory {
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), StorageError> {
        if !guest_range(address, bytes.len() as u64) {
            return Err(StorageError);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len());
        }
        Ok(())
    }

    fn zero(&mut self, address: u64, length: u64) -> Result<(), StorageError> {
        if !guest_range(address, length) {
            return Err(StorageError);
        }
        unsafe {
            core::ptr::write_bytes(address as *mut u8, 0, length as usize);
        }
        Ok(())
    }
}

pub fn initialize(
    enabled: bool,
    console_enabled: bool,
    require_upload: bool,
    gdb_enabled: bool,
    guest_entry: u64,
) -> Result<bool, Error> {
    ENABLED.store(enabled, Ordering::Release);
    AVAILABLE.store(false, Ordering::Release);
    REQUIRE_UPLOAD.store(require_upload, Ordering::Release);
    GDB_ENABLED.store(gdb_enabled, Ordering::Release);
    GUEST_RUNNING.store(false, Ordering::Release);
    GUEST_ENTRY.store(guest_entry, Ordering::Release);
    RESET.store(RESET_NONE, Ordering::Release);
    guest_console::configure(console_enabled);
    if !enabled {
        return Ok(false);
    }
    // QEMU CPU tests have no Tegra USB IP. Never touch absent or unpowered XUDC.
    if (Hardware.read32(0x7000_0804) >> 8) & 0xff != 0x21 {
        return Ok(false);
    }
    unsafe {
        USB.with(|state| {
            state.control = Parser::new();
            state.control_output = Output::new();
            state.loader = Loader::new();
            state.loader_output = Output::new();
            state.gdb_rx = Fifo::new();
            state.gdb_tx = Fifo::new();
            state.boot = None;
            state.error = None;
            state.driver.set_gdb_enabled(gdb_enabled);
            state.driver.initialize()?;
            state.gdb_reset_count = state.driver.reset_count();
            Ok(())
        })?;
    }
    AVAILABLE.store(true, Ordering::Release);
    guest_console::set_transport_available(true);
    Ok(true)
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn available() -> bool {
    AVAILABLE.load(Ordering::Acquire)
}

pub fn gdb_connected() -> bool {
    GDB_ENABLED.load(Ordering::Acquire)
        && available()
        && unsafe { USB.with(|state| state.driver.connected_channel(Channel::Gdb)) }
}

pub fn gdb_read(bytes: &mut [u8]) -> usize {
    if !available() {
        return 0;
    }
    unsafe { USB.with(|state| state.gdb_rx.pop(bytes)) }
}

pub fn gdb_write(bytes: &[u8]) -> usize {
    if !available() {
        return 0;
    }
    unsafe { USB.with(|state| state.gdb_tx.push(bytes)) }
}

/// Queue GDB output before CPU0 returns to a guest that may make no exits.
pub fn gdb_flush() {
    if !available() {
        return;
    }
    let asserted = unsafe {
        USB.with(|state| {
            LAST_SERVICE.store(Hardware.now_us(), Ordering::Release);
            service_locked(state);
            guest_console::interrupt_pending()
        })
    };
    if asserted {
        crate::arch::aarch64::interrupt::pend_console_interrupt();
    }
    reset_if_requested();
}

pub fn enter_guest(entry: u64) {
    GUEST_ENTRY.store(entry, Ordering::Release);
    GUEST_RUNNING.store(true, Ordering::Release);
    if available() {
        unsafe { USB.with(|state| state.loader.disable()) }
    }
}

pub fn service() {
    if !available() {
        return;
    }
    let last = LAST_SERVICE.load(Ordering::Acquire);
    if Hardware.now_us().wrapping_sub(last) < POLL_INTERVAL_US {
        return;
    }
    let asserted = unsafe {
        USB.with(|state| {
            let now = Hardware.now_us();
            if now.wrapping_sub(LAST_SERVICE.load(Ordering::Acquire)) < POLL_INTERVAL_US {
                return guest_console::interrupt_pending();
            }
            LAST_SERVICE.store(now, Ordering::Release);
            service_locked(state);
            guest_console::interrupt_pending()
        })
    };
    if asserted {
        crate::arch::aarch64::interrupt::pend_console_interrupt();
    }
    reset_if_requested();
}

/// Service a physical XUDC event immediately and return the virtual UART line.
pub fn service_interrupt() -> bool {
    if !available() {
        return false;
    }
    let asserted = unsafe {
        USB.with(|state| {
            LAST_SERVICE.store(Hardware.now_us(), Ordering::Release);
            service_locked(state);
            guest_console::interrupt_pending()
        })
    };
    reset_if_requested();
    asserted
}

fn service_locked(state: &mut Service) {
    if let Err(error) = state.driver.poll() {
        state.error = Some(error);
    }
    service_guest_console(state);
    service_control(state);
    service_loader(state);
    if GDB_ENABLED.load(Ordering::Acquire) {
        service_gdb(state);
    }
}

fn service_gdb(state: &mut Service) {
    let reset_count = state.driver.reset_count();
    if reset_count != state.gdb_reset_count {
        state.gdb_reset_count = reset_count;
        state.gdb_rx = Fifo::new();
        state.gdb_tx = Fifo::new();
        if crate::gdb::active() {
            crate::debug::request_disconnect();
        }
    }
    if !state.driver.connected_channel(Channel::Gdb) {
        if crate::gdb::active() {
            crate::debug::request_disconnect();
        } else {
            crate::debug::cancel_attach();
        }
        state.gdb_rx = Fifo::new();
        state.gdb_tx = Fifo::new();
        return;
    }
    let mut bytes = [0; 512];
    let capacity = state.gdb_rx.capacity().min(bytes.len());
    if capacity != 0 {
        match state
            .driver
            .receive_channel(Channel::Gdb, &mut bytes[..capacity])
        {
            Ok(n) if n != 0 => {
                state.gdb_rx.push(&bytes[..n]);
                if !crate::gdb::active() {
                    crate::debug::request_attach();
                } else if !crate::debug::world_stopped() && bytes[..n].contains(&3) {
                    crate::debug::request_interrupt();
                }
            }
            Ok(_) => (),
            Err(error) => state.error = Some(error),
        }
    }
    let capacity = state
        .driver
        .send_capacity_channel(Channel::Gdb)
        // Keep each RSP fragment below the high-speed max packet size. Large
        // memory replies otherwise force an intervening CDC zero-length packet.
        .min(256)
        .min(bytes.len());
    if capacity != 0 {
        let count = state.gdb_tx.peek(&mut bytes[..capacity]);
        if count != 0 {
            match state.driver.send_channel(Channel::Gdb, &bytes[..count]) {
                Ok(sent) => {
                    let _ = state.gdb_tx.pop(&mut bytes[..sent]);
                }
                Err(error) => state.error = Some(error),
            }
        }
    }
}

fn service_guest_console(state: &mut Service) {
    let mut bytes = [0; 512];
    let capacity = guest_console::receive_capacity().min(bytes.len());
    if capacity != 0 {
        match state
            .driver
            .receive_channel(Channel::Console, &mut bytes[..capacity])
        {
            Ok(received) => {
                assert_eq!(guest_console::receive(&bytes[..received]), received);
            }
            Err(error) => state.error = Some(error),
        }
    }
    let capacity = state
        .driver
        .send_capacity_channel(Channel::Console)
        .min(bytes.len());
    if capacity != 0 {
        let length = guest_console::peek_tx(&mut bytes[..capacity]);
        match state
            .driver
            .send_channel(Channel::Console, &bytes[..length])
        {
            Ok(accepted) => guest_console::consume_tx(accepted),
            Err(error) => state.error = Some(error),
        }
    }
}

fn service_control(state: &mut Service) {
    if !state.control_output.pending().is_empty() {
        transmit_output(
            &mut state.driver,
            Channel::Control,
            &mut state.control_output,
            &mut state.error,
        );
        return;
    }
    let mut byte = [0];
    loop {
        match state.driver.receive_channel(Channel::Control, &mut byte) {
            Ok(0) => break,
            Ok(1) => {
                if let Some(command) = state.control.push(byte[0]) {
                    control_reply(state, command);
                    break;
                }
            }
            Ok(_) => {
                state.error = Some(Error::Transfer);
                return;
            }
            Err(error) => {
                state.error = Some(error);
                return;
            }
        }
    }
    transmit_output(
        &mut state.driver,
        Channel::Control,
        &mut state.control_output,
        &mut state.error,
    );
}

fn control_reply(state: &mut Service, command: Result<Command, ParseError>) {
    match command {
        Ok(Command::Ping) => {
            let _ = writeln!(
                state.control_output,
                "switchvisor {}",
                env!("CARGO_PKG_VERSION")
            );
            let _ = writeln!(state.control_output, "OK");
        }
        Ok(Command::Status) => {
            let boot = if GUEST_RUNNING.load(Ordering::Acquire) {
                "guest-running"
            } else {
                "preboot"
            };
            let loader = match state.loader.state() {
                LoaderState::Idle => "idle",
                LoaderState::Bundle => "bundle",
                LoaderState::Receiving => "receiving",
                LoaderState::Ready => "ready",
                LoaderState::Disabled => "disabled",
            };
            let _ = writeln!(state.control_output, "state={boot}");
            let _ = writeln!(state.control_output, "cpu-mask={:#x}", vcpu::cpu_mask());
            let _ = writeln!(state.control_output, "usb=up");
            let snapshot = state.driver.snapshot();
            let stats = &state.driver.statistics;
            let _ = writeln!(state.control_output, "usb-errors={}", stats.errors);
            let _ = writeln!(
                state.control_output,
                "usb-dtr={}/{}/{}",
                u8::from(snapshot.dtr),
                u8::from(snapshot.control_dtr),
                u8::from(snapshot.gdb_dtr)
            );
            let _ = writeln!(
                state.control_output,
                "usb-endpoints={:08x},{:08x},{:x},{:x},{:x}",
                snapshot.endpoint_halt,
                snapshot.endpoint_pause,
                snapshot.out_armed,
                snapshot.rx_pending,
                snapshot.tx_pending
            );
            let _ = writeln!(
                state.control_output,
                "usb-data={},{},{},{}",
                stats.received, stats.transmitted, stats.dropped, stats.events
            );
            let rx = stats.channel_received;
            let tx = stats.channel_transmitted;
            let _ = writeln!(
                state.control_output,
                "usb-rx={},{},{},{}",
                rx[0], rx[1], rx[2], rx[3]
            );
            let _ = writeln!(
                state.control_output,
                "usb-tx={},{},{},{}",
                tx[0], tx[1], tx[2], tx[3]
            );
            let _ = writeln!(
                state.control_output,
                "usb-last-event={:08x},{:08x},{:08x},{:08x}",
                stats.last_event[0], stats.last_event[1], stats.last_event[2], stats.last_event[3]
            );
            let _ = writeln!(state.control_output, "loader={loader}");
            let fallback = if REQUIRE_UPLOAD.load(Ordering::Acquire) {
                "disabled"
            } else {
                "enabled"
            };
            let _ = writeln!(state.control_output, "fallback={fallback}");
            let _ = writeln!(state.control_output, "gdb={}", crate::debug::status());
            let _ = writeln!(
                state.control_output,
                "gdb-stop-cpu={}",
                crate::debug::last_stop_cpu()
            );
            let _ = writeln!(
                state.control_output,
                "gdb-stop-reason={}",
                crate::debug::last_stop_cause()
            );
            let stop_cpu = crate::debug::last_stop_cpu() as usize;
            if stop_cpu < switchvisor::CPU_COUNT {
                let _ = writeln!(
                    state.control_output,
                    "gdb-stop-esr={:#x}",
                    crate::debug::stop_esr(stop_cpu)
                );
            }
            let _ = writeln!(
                state.control_output,
                "gdb-missing-mask={:#x}",
                crate::debug::last_missing_mask()
            );
            let _ = writeln!(
                state.control_output,
                "gdb-breakpoints={}",
                crate::gdb::breakpoint_count()
            );
            let _ = writeln!(
                state.control_output,
                "guest-entry={:#x}",
                GUEST_ENTRY.load(Ordering::Acquire)
            );
            let _ = writeln!(state.control_output, "OK");
        }
        Ok(Command::Reboot) => {
            let _ = writeln!(state.control_output, "OK rebooting");
            RESET.store(RESET_NORMAL, Ordering::Release);
        }
        Ok(Command::RebootRcm) => {
            let _ = writeln!(state.control_output, "OK rebooting-rcm");
            RESET.store(RESET_RCM, Ordering::Release);
        }
        Err(ParseError::InvalidEncoding) => {
            let _ = writeln!(state.control_output, "ERR invalid-encoding");
        }
        Err(ParseError::LineTooLong) => {
            let _ = writeln!(state.control_output, "ERR line-too-long");
        }
        Err(ParseError::UnknownCommand) => {
            let _ = writeln!(state.control_output, "ERR unknown-command");
        }
    }
}

fn service_loader(state: &mut Service) {
    if !state.loader_output.pending().is_empty() {
        transmit_output(
            &mut state.driver,
            Channel::Loader,
            &mut state.loader_output,
            &mut state.error,
        );
        return;
    }
    let received = match state
        .driver
        .receive_channel(Channel::Loader, &mut state.loader_input)
    {
        Ok(received) => received,
        Err(error) => {
            state.error = Some(error);
            return;
        }
    };
    if received == 0 {
        return;
    }
    if let Ok((response, action)) = state
        .loader
        .handle(&state.loader_input[..received], &mut GuestMemory)
    {
        let mut encoded = [0; MAX_RESPONSE_SIZE];
        let length = response.encode(&mut encoded);
        assert!(state.loader_output.append(&encoded[..length]).is_ok());
        if let Action::Boot(descriptor) = action {
            GUEST_ENTRY.store(descriptor.entry, Ordering::Release);
            state.boot = Some(descriptor);
        }
        transmit_output(
            &mut state.driver,
            Channel::Loader,
            &mut state.loader_output,
            &mut state.error,
        );
    }
}

fn transmit_output<const N: usize>(
    driver: &mut Xudc<Hardware, UsbDma>,
    channel: Channel,
    output: &mut Output<N>,
    error: &mut Option<Error>,
) {
    if output.pending().is_empty() {
        return;
    }
    match driver.send_channel(channel, output.pending()) {
        Ok(sent) => output.consume(sent),
        Err(value) => *error = Some(value),
    }
}

pub fn preboot(screen: &mut impl Write) -> Option<BundleDescriptor> {
    let require_upload = REQUIRE_UPLOAD.load(Ordering::Acquire);
    if !available() {
        if require_upload {
            let _ = writeln!(screen, "USB BUNDLE REQUIRED - USB UNAVAILABLE\nCPU0 PARKED");
            park();
        }
        return None;
    }
    if require_upload {
        let _ = writeln!(screen, "USB PREBOOT - BUNDLE REQUIRED");
    } else {
        let _ = writeln!(screen, "USB PREBOOT WINDOW (2S)");
    }
    let start = Hardware.now_us();
    loop {
        service();
        let (boot, claimed) =
            unsafe { USB.with(|state| (state.boot.take(), state.loader.claimed())) };
        if let Some(descriptor) = boot {
            diagnostics(screen);
            return Some(descriptor);
        }
        if !require_upload && !claimed && Hardware.now_us().wrapping_sub(start) >= PREBOOT_WINDOW_US
        {
            diagnostics(screen);
            return None;
        }
        Hardware.delay_us(100);
    }
}

fn diagnostics(screen: &mut impl Write) {
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
                "USB CTRL={:08x} EPHALT={:08x} PAUSE={:08x} EP0={} CFG={} DTR={}/{}",
                status.control,
                status.endpoint_halt,
                status.endpoint_pause,
                status.ep0_state,
                status.configuration,
                u8::from(status.dtr),
                u8::from(status.control_dtr)
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

/// Serialize shared USB clock/power MMIO with driver polling and other vCPUs.
/// Callers must satisfy the masked, non-reentrant EL2 mutex preconditions.
pub unsafe fn with_io<R>(operation: impl FnOnce(&mut Hardware) -> R) -> R {
    unsafe { USB.with(|_| operation(&mut Hardware)) }
}

fn reset_if_requested() {
    match RESET.load(Ordering::Acquire) {
        RESET_NORMAL => reset(false),
        RESET_RCM => reset(true),
        _ => (),
    }
}

fn reset(rcm: bool) -> ! {
    if rcm {
        Hardware.write32(PMC + 0x50, 1 << 1);
    }
    Hardware.barrier();
    let control = Hardware.read32(PMC);
    Hardware.write32(PMC, control | (1 << 4));
    Hardware.barrier();
    unsafe {
        asm!("dsb sy", options(nostack));
    }
    park()
}
