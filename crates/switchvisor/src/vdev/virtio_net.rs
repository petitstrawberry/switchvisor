//! Virtio 1.2 network device over modern MMIO (version 2), split queues.
//! Only MAC, link status and VERSION_1 are offered. Guest descriptors are
//! snapshotted and validated before copying; no guest pointers are retained.
use super::{DeviceError, MmioRegion, VirtualDevice};
use crate::net::{FRAME_SIZE, GUEST_MAC};

pub const BASE: u64 = 0x700f_e000;
pub const SIZE: u64 = 4096;
// Reuse the EL2-owned, disabled XUSB host source as a separate virtual line.
pub const INTERRUPT_ID: u32 = 32 + 39;
pub const QUEUE_SIZE: usize = 128;
pub const HEADER_SIZE: usize = 12;
const FEATURES: u64 = (1 << 5) | (1 << 16) | (1 << 32);
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
const NEEDS_RESET: u32 = 64;
const FAILED: u32 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryError;

/// Implementations must check *every* read/write and preserve ordering against
/// the guest's shared RAM, including across CPUs. `valid` excludes EL2 and MMIO.
pub trait GuestMemory {
    fn valid(&self, address: u64, length: usize) -> bool;
    fn read(&mut self, address: u64, output: &mut [u8]) -> Result<(), MemoryError>;
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryError>;
    fn barrier(&mut self);
}

#[derive(Clone, Copy)]
struct Queue {
    num: u32,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    next_avail: u16,
    next_used: u16,
}
impl Queue {
    const fn new() -> Self {
        Self {
            num: 0,
            ready: false,
            desc: 0,
            avail: 0,
            used: 0,
            next_avail: 0,
            next_used: 0,
        }
    }
    fn valid(&self, memory: &impl GuestMemory) -> bool {
        self.num != 0
            && self.num.is_power_of_two()
            && self.num as usize <= QUEUE_SIZE
            && self.desc % 16 == 0
            && self.avail % 2 == 0
            && self.used % 4 == 0
            && memory.valid(self.desc, self.num as usize * 16)
            && memory.valid(self.avail, 6 + self.num as usize * 2)
            && memory.valid(self.used, 6 + self.num as usize * 8)
    }
    fn next(
        &self,
        memory: &mut impl GuestMemory,
        writable: bool,
    ) -> Result<Option<Chain>, MemoryError> {
        if !self.ready {
            return Ok(None);
        }
        if !self.valid(memory) {
            return Err(MemoryError);
        }
        let available = read16(memory, self.avail + 2)?;
        memory.barrier();
        let count = available.wrapping_sub(self.next_avail);
        if count == 0 {
            return Ok(None);
        }
        if u32::from(count) > self.num {
            return Err(MemoryError);
        }
        let head = read16(
            memory,
            self.avail + 4 + u64::from(self.next_avail % self.num as u16) * 2,
        )?;
        let mut chain = Chain {
            head,
            segments: [Segment::EMPTY; QUEUE_SIZE],
            count: 0,
            length: 0,
        };
        let mut index = head;
        let mut seen = [0u64; QUEUE_SIZE / 64];
        loop {
            if u32::from(index) >= self.num {
                return Err(MemoryError);
            }
            let slot = usize::from(index);
            if seen[slot / 64] & (1 << (slot % 64)) != 0 {
                return Err(MemoryError);
            }
            seen[slot / 64] |= 1 << (slot % 64);
            let mut descriptor = [0; 16];
            memory.read(self.desc + u64::from(index) * 16, &mut descriptor)?;
            let address = u64::from_le_bytes(descriptor[..8].try_into().unwrap());
            let length = u32::from_le_bytes(descriptor[8..12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(descriptor[12..14].try_into().unwrap());
            if flags & !3 != 0
                || (flags & 2 != 0) != writable
                || length == 0
                || !memory.valid(address, length)
            {
                return Err(MemoryError);
            }
            chain.length = chain.length.checked_add(length).ok_or(MemoryError)?;
            chain.segments[chain.count] = Segment { address, length };
            chain.count += 1;
            if flags & 1 == 0 {
                break;
            }
            index = u16::from_le_bytes(descriptor[14..16].try_into().unwrap());
        }
        Ok(Some(chain))
    }
    fn finish(
        &mut self,
        memory: &mut impl GuestMemory,
        head: u16,
        length: usize,
    ) -> Result<bool, MemoryError> {
        let offset = self.used + 4 + u64::from(self.next_used % self.num as u16) * 8;
        let mut entry = [0; 8];
        entry[..4].copy_from_slice(&u32::from(head).to_le_bytes());
        entry[4..].copy_from_slice(&(length as u32).to_le_bytes());
        memory.write(offset, &entry)?;
        memory.barrier(); // Publish packet and used entry before used.idx.
        self.next_used = self.next_used.wrapping_add(1);
        memory.write(self.used + 2, &self.next_used.to_le_bytes())?;
        self.next_avail = self.next_avail.wrapping_add(1);
        memory.barrier();
        Ok(read16(memory, self.avail)? & 1 == 0)
    }
}

fn read16(memory: &mut impl GuestMemory, address: u64) -> Result<u16, MemoryError> {
    let mut bytes = [0; 2];
    memory.read(address, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

#[derive(Clone, Copy)]
struct Segment {
    address: u64,
    length: usize,
}
impl Segment {
    const EMPTY: Self = Self {
        address: 0,
        length: 0,
    };
}
struct Chain {
    head: u16,
    segments: [Segment; QUEUE_SIZE],
    count: usize,
    length: usize,
}
impl Chain {
    fn read(&self, memory: &mut impl GuestMemory, bytes: &mut [u8]) -> Result<(), MemoryError> {
        let mut cursor = 0;
        for segment in &self.segments[..self.count] {
            let count = segment.length.min(bytes.len() - cursor);
            memory.read(segment.address, &mut bytes[cursor..cursor + count])?;
            cursor += count;
            if cursor == bytes.len() {
                break;
            }
        }
        Ok(())
    }
    fn write(&self, memory: &mut impl GuestMemory, bytes: &[u8]) -> Result<(), MemoryError> {
        let mut cursor = 0;
        for segment in &self.segments[..self.count] {
            let count = segment.length.min(bytes.len() - cursor);
            memory.write(segment.address, &bytes[cursor..cursor + count])?;
            cursor += count;
            if cursor == bytes.len() {
                break;
            }
        }
        Ok(())
    }
}

pub struct Net {
    queues: [Queue; 2],
    queue_sel: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    status: u32,
    interrupt: u32,
    generation: u8,
    link: bool,
    pub tx_packets: u64,
    pub rx_packets: u64,
    pub errors: u64,
}
impl Default for Net {
    fn default() -> Self {
        Self::new()
    }
}
impl Net {
    pub const fn new() -> Self {
        Self {
            queues: [Queue::new(); 2],
            queue_sel: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            status: 0,
            interrupt: 0,
            generation: 0,
            link: false,
            tx_packets: 0,
            rx_packets: 0,
            errors: 0,
        }
    }
    pub fn interrupt_pending(&self) -> bool {
        self.interrupt != 0
    }
    pub fn running(&self) -> bool {
        self.status & (DRIVER_OK | FEATURES_OK | NEEDS_RESET | FAILED) == DRIVER_OK | FEATURES_OK
    }
    pub fn set_link(&mut self, up: bool) {
        if self.link != up {
            self.link = up;
            self.generation = self.generation.wrapping_add(1);
            if self.running() && self.driver_features & (1 << 16) != 0 {
                self.interrupt |= 2;
            }
        }
    }
    fn broken(&mut self) {
        self.status |= NEEDS_RESET;
        self.interrupt |= 2;
        self.errors = self.errors.saturating_add(1);
    }
    /// Called only when downstream has reserved room for one complete frame.
    pub fn transmit(
        &mut self,
        memory: &mut impl GuestMemory,
        frame: &mut [u8; FRAME_SIZE],
    ) -> usize {
        if !self.running() {
            return 0;
        }
        match self.transmit_inner(memory, frame) {
            Ok(length) => length,
            Err(_) => {
                self.broken();
                0
            }
        }
    }
    fn transmit_inner(
        &mut self,
        memory: &mut impl GuestMemory,
        frame: &mut [u8; FRAME_SIZE],
    ) -> Result<usize, MemoryError> {
        let queue = &mut self.queues[1];
        let Some(chain) = queue.next(memory, false)? else {
            return Ok(0);
        };
        if !(HEADER_SIZE + 14..=HEADER_SIZE + FRAME_SIZE).contains(&chain.length) {
            return Err(MemoryError);
        }
        let mut packet = [0; HEADER_SIZE + FRAME_SIZE];
        chain.read(memory, &mut packet[..chain.length])?;
        if packet[0] != 0 || packet[1] != 0 || packet[10..12] != [0, 0] {
            return Err(MemoryError);
        }
        let length = chain.length - HEADER_SIZE;
        frame[..length].copy_from_slice(&packet[HEADER_SIZE..chain.length]);
        if queue.finish(memory, chain.head, 0)? {
            self.interrupt |= 1;
        }
        self.tx_packets = self.tx_packets.saturating_add(1);
        Ok(length)
    }
    pub fn receive(&mut self, memory: &mut impl GuestMemory, frame: &[u8]) -> bool {
        if !self.running() || !(14..=FRAME_SIZE).contains(&frame.len()) {
            return false;
        }
        match self.receive_inner(memory, frame) {
            Ok(accepted) => accepted,
            Err(_) => {
                self.broken();
                false
            }
        }
    }
    fn receive_inner(
        &mut self,
        memory: &mut impl GuestMemory,
        frame: &[u8],
    ) -> Result<bool, MemoryError> {
        let queue = &mut self.queues[0];
        let Some(chain) = queue.next(memory, true)? else {
            return Ok(false);
        };
        let length = HEADER_SIZE + frame.len();
        if chain.length < length {
            return Err(MemoryError);
        }
        let mut packet = [0; HEADER_SIZE + FRAME_SIZE];
        packet[10] = 1; // num_buffers is present even without MRG_RXBUF in v1+.
        packet[HEADER_SIZE..length].copy_from_slice(frame);
        chain.write(memory, &packet[..length])?;
        if queue.finish(memory, chain.head, length)? {
            self.interrupt |= 1;
        }
        self.rx_packets = self.rx_packets.saturating_add(1);
        Ok(true)
    }
}

impl VirtualDevice for Net {
    fn region(&self) -> MmioRegion {
        MmioRegion {
            base: BASE,
            size: SIZE,
        }
    }
    fn read(&mut self, offset: u64, size: u8) -> Result<u64, DeviceError> {
        if (0x100..0x108).contains(&offset) {
            if !matches!(size, 1 | 2 | 4)
                || offset + u64::from(size) > 0x108
                || offset % u64::from(size) != 0
            {
                return Err(DeviceError::AccessSize);
            }
            let mut config = [0u8; 8];
            config[..6].copy_from_slice(&GUEST_MAC);
            config[6] = u8::from(self.link);
            let mut value = 0u64;
            for i in 0..usize::from(size) {
                value |= u64::from(config[offset as usize - 0x100 + i]) << (i * 8);
            }
            return Ok(value);
        }
        if size != 4 || offset % 4 != 0 || offset >= SIZE {
            return Err(DeviceError::AccessSize);
        }
        let queue = self.queues.get(self.queue_sel as usize);
        Ok(u64::from(match offset {
            0x00 => 0x7472_6976,
            0x04 => 2,
            0x08 => 1,
            0x0c => 0x5357_5652,
            0x10 => {
                if self.device_features_sel < 2 {
                    (FEATURES >> (self.device_features_sel * 32)) as u32
                } else {
                    0
                }
            }
            0x34 => {
                if queue.is_some() {
                    QUEUE_SIZE as u32
                } else {
                    0
                }
            }
            0x44 => queue.map_or(0, |q| u32::from(q.ready)),
            0x60 => self.interrupt,
            0x70 => self.status,
            0xfc => u32::from(self.generation),
            _ => 0,
        }))
    }
    fn write(&mut self, offset: u64, size: u8, value: u64) -> Result<(), DeviceError> {
        if size != 4 || offset % 4 != 0 || offset >= SIZE {
            return Err(DeviceError::AccessSize);
        }
        let value = value as u32;
        match offset {
            0x14 => self.device_features_sel = value,
            0x24 => self.driver_features_sel = value,
            0x20 if self.status & FEATURES_OK == 0 && self.driver_features_sel < 2 => {
                let shift = self.driver_features_sel * 32;
                self.driver_features = (self.driver_features & !(0xffff_ffffu64 << shift))
                    | (u64::from(value) << shift);
            }
            0x30 => self.queue_sel = value,
            0x64 => self.interrupt &= !(value & 3),
            0x70 => {
                if value == 0 {
                    let link = self.link;
                    *self = Self::new();
                    self.link = link;
                } else {
                    let mut next = value | (self.status & NEEDS_RESET);
                    if next & FEATURES_OK != 0
                        && (self.driver_features & !FEATURES != 0
                            || self.driver_features & (1 << 32) == 0)
                    {
                        next &= !FEATURES_OK;
                    }
                    if next & (FEATURES_OK | 3) != FEATURES_OK | 3 {
                        next &= !DRIVER_OK;
                    }
                    self.status = next & 0xcf;
                }
            }
            0x38 | 0x44 | 0x80 | 0x84 | 0x90 | 0x94 | 0xa0 | 0xa4 => {
                if let Some(queue) = self.queues.get_mut(self.queue_sel as usize) {
                    if offset == 0x44 {
                        if value == 0 {
                            queue.ready = false;
                        } else if value == 1 {
                            queue.ready = true;
                        }
                    } else if !queue.ready {
                        match offset {
                            0x38 => queue.num = value,
                            0x80 => queue.desc = (queue.desc & !0xffff_ffff) | u64::from(value),
                            0x84 => {
                                queue.desc = (queue.desc & 0xffff_ffff) | (u64::from(value) << 32)
                            }
                            0x90 => queue.avail = (queue.avail & !0xffff_ffff) | u64::from(value),
                            0x94 => {
                                queue.avail = (queue.avail & 0xffff_ffff) | (u64::from(value) << 32)
                            }
                            0xa0 => queue.used = (queue.used & !0xffff_ffff) | u64::from(value),
                            0xa4 => {
                                queue.used = (queue.used & 0xffff_ffff) | (u64::from(value) << 32)
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {} // QueueNotify is a hint; service observes both queues.
        }
        Ok(())
    }
}
