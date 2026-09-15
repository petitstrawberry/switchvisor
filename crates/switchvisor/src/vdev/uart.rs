//! Guest-visible NS16550A with byte-spaced registers and bounded RX/TX queues.
use super::{DeviceError, MmioRegion, VirtualDevice};

pub const BASE: u64 = 0x700f_f000;
pub const SIZE: u64 = 4096;
pub const PATH: &str = "/serial@700ff000";
/// Tegra210 XUSB device SPI 44, reused after physical USB is hidden from EL1.
pub const INTERRUPT_ID: u32 = 32 + 44;
pub const TX_CAPACITY: usize = 65536;
pub const RX_CAPACITY: usize = 4096;

pub struct Uart {
    divisor: [u8; 2],
    ier: u8,
    lcr: u8,
    mcr: u8,
    fifo: bool,
    scratch: u8,
    overrun: bool,
    rx: RxQueue<RX_CAPACITY>,
    tx: TxQueue<TX_CAPACITY>,
}

impl Default for Uart {
    fn default() -> Self {
        Self::new()
    }
}

impl Uart {
    pub const fn new() -> Self {
        Self {
            divisor: [0; 2],
            ier: 0,
            lcr: 0,
            mcr: 0,
            fifo: false,
            scratch: 0,
            overrun: false,
            rx: RxQueue::new(),
            tx: TxQueue::new(),
        }
    }

    pub fn tx(&mut self) -> &mut TxQueue<TX_CAPACITY> {
        &mut self.tx
    }

    pub fn receive(&mut self, bytes: &[u8]) -> usize {
        let count = bytes.len().min(self.rx.capacity());
        for &byte in &bytes[..count] {
            assert!(self.rx.push(byte));
        }
        if count != bytes.len() {
            self.overrun = true;
        }
        count
    }

    pub fn receive_capacity(&self) -> usize {
        self.rx.capacity()
    }

    pub fn interrupt_pending(&self) -> bool {
        (self.ier & 1 != 0 && !self.rx.is_empty()) || (self.ier & 4 != 0 && self.overrun)
    }

    fn read_register(&mut self, offset: u64) -> Option<u8> {
        Some(match offset {
            0 if self.lcr & 0x80 != 0 => self.divisor[0],
            1 if self.lcr & 0x80 != 0 => self.divisor[1],
            0 => self.rx.pop().unwrap_or(0),
            1 => self.ier,
            2 => {
                let fifo = if self.fifo { 0xc0 } else { 0 };
                if self.ier & 4 != 0 && self.overrun {
                    fifo | 0x06
                } else if self.ier & 1 != 0 && !self.rx.is_empty() {
                    fifo | 0x04
                } else {
                    fifo | 1
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                let status = 0x60 | u8::from(!self.rx.is_empty()) | (u8::from(self.overrun) << 1);
                self.overrun = false;
                status
            }
            6 if self.mcr & 0x10 != 0 => {
                ((self.mcr & 2) << 3)
                    | ((self.mcr & 1) << 5)
                    | ((self.mcr & 4) << 4)
                    | ((self.mcr & 8) << 4)
            }
            6 => 0xb0, // CTS, DSR and carrier asserted.
            7 => self.scratch,
            _ => return None,
        })
    }

    /// THR is accepted immediately; receive interrupt and FIFO state are modeled.
    fn write_register(&mut self, offset: u64, value: u8) -> Result<(), DeviceError> {
        match offset {
            0 | 1 if self.lcr & 0x80 != 0 => self.divisor[offset as usize] = value,
            0 => self.tx.push(value),
            // RDA and receiver-line-status are supported. THRE remains polling-only.
            1 => self.ier = value & 0x05,
            2 => {
                self.fifo = value & 1 != 0;
                if value & 2 != 0 {
                    self.rx.clear();
                    self.overrun = false;
                }
            }
            3 => self.lcr = value,
            4 => self.mcr = value & 0x1f,
            5 | 6 => (),
            7 => self.scratch = value,
            _ => return Err(DeviceError::Register),
        }
        Ok(())
    }
}

impl VirtualDevice for Uart {
    fn region(&self) -> MmioRegion {
        MmioRegion {
            base: BASE,
            size: SIZE,
        }
    }
    fn read(&mut self, offset: u64, size: u8) -> Result<u64, DeviceError> {
        if size != 1 {
            return Err(DeviceError::AccessSize);
        }
        self.read_register(offset)
            .map(u64::from)
            .ok_or(DeviceError::Register)
    }
    fn write(&mut self, offset: u64, size: u8, value: u64) -> Result<(), DeviceError> {
        if size != 1 {
            return Err(DeviceError::AccessSize);
        }
        self.write_register(offset, value as u8)
    }
}

struct RxQueue<const N: usize> {
    bytes: [u8; N],
    head: usize,
    len: usize,
}

impl<const N: usize> RxQueue<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, byte: u8) -> bool {
        if self.len == N {
            return false;
        }
        self.bytes[(self.head + self.len) % N] = byte;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.bytes[self.head];
        if N != 0 {
            self.head = (self.head + 1) % N;
        }
        self.len -= 1;
        Some(byte)
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    const fn capacity(&self) -> usize {
        N - self.len
    }

    const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Bounded wire-side log queue. Disconnected hosts cannot stall guest THR writes.
pub struct TxQueue<const N: usize> {
    bytes: [u8; N],
    head: usize,
    len: usize,
    dropped: u64,
}

impl<const N: usize> Default for TxQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> TxQueue<N> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }
    pub fn push(&mut self, byte: u8) {
        if self.len == N {
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.bytes[(self.head + self.len) % N] = byte;
            self.len += 1;
        }
    }
    pub fn peek(&self, output: &mut [u8]) -> usize {
        let count = self.len.min(output.len());
        for (index, byte) in output[..count].iter_mut().enumerate() {
            *byte = self.bytes[(self.head + index) % N];
        }
        count
    }
    pub fn consume(&mut self, count: usize) {
        let count = count.min(self.len);
        if N != 0 {
            self.head = (self.head + count) % N;
        }
        self.len -= count;
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
