//! Guest-visible transmit-only NS16550A, byte-spaced registers and no interrupt line.
use super::{DeviceError, MmioRegion, VirtualDevice};

pub const BASE: u64 = 0x700f_f000;
pub const SIZE: u64 = 4096;
pub const PATH: &str = "/serial@700ff000";
pub const TX_CAPACITY: usize = 65536;

pub struct Uart {
    divisor: [u8; 2],
    lcr: u8,
    mcr: u8,
    fifo: bool,
    scratch: u8,
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
            lcr: 0,
            mcr: 0,
            fifo: false,
            scratch: 0,
            tx: TxQueue::new(),
        }
    }

    pub fn tx(&mut self) -> &mut TxQueue<TX_CAPACITY> {
        &mut self.tx
    }

    /// The UART has no receive data and no pending interrupt, even if IER is written.
    fn read_register(&self, offset: u64) -> Option<u8> {
        Some(match offset {
            0 if self.lcr & 0x80 != 0 => self.divisor[0],
            1 if self.lcr & 0x80 != 0 => self.divisor[1],
            0 | 1 => 0,
            2 => {
                if self.fifo {
                    0xc1
                } else {
                    1
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => 0x60, // THRE | TEMT; the transport never backpressures the guest.
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

    /// FIFO reset has no wire-side effect: THR is accepted immediately.
    fn write_register(&mut self, offset: u64, value: u8) -> Result<(), DeviceError> {
        match offset {
            0 | 1 if self.lcr & 0x80 != 0 => self.divisor[offset as usize] = value,
            0 => self.tx.push(value),
            1 | 5 | 6 => (),
            2 => self.fifo = value & 1 != 0,
            3 => self.lcr = value,
            4 => self.mcr = value & 0x1f,
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
