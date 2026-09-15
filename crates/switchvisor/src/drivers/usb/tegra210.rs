//! Tegra210 XUDC, USB 2.0 CDC ACM with event IRQs and resident DRAM DMA buffers.
//!
//! Adapted from Hekate bdk/usb/xusbd.c and bdk/soc/clock.c at
//! e487de8fdd6ca9c3f608d1d18c097a86355912b9.
//! Copyright (c) 2020-2025 CTCaer (XUDC implementation).
//! Copyright (c) 2018 naehrwert; 2018-2026 CTCaer (clock implementation).
//! SPDX-License-Identifier: GPL-2.0-only

use super::cdc::{Acm, CONTROL_SIZE, Reply, Setup};
use crate::{
    drivers::{Clock, DmaBuffer, Driver, Mmio, RxTransport, TxTransport},
    payload::{RESIDENT_BASE, RESIDENT_SIZE},
};

pub const CAR: u64 = 0x6000_6000;
pub const PMC: u64 = 0x7000_e400;
pub const DEV_ASID: u64 = crate::mc::BASE + 0x28c;
pub const DEV: u64 = 0x700d_0000;
const PAD: u64 = 0x7009_f000;
const VBUS_ON: u32 = 1 << 14;
pub const DMA_SIZE: usize = 8192;
const EVENT0: usize = 0;
const EVENT1: usize = 0x100;
const CONTEXT: usize = 0x800;
const CONTROL: usize = 0xa00;
const TX_BUFFER: usize = 0xc00;
const RX_BUFFER: usize = 0xe00;
const NOTIFY_BUFFER: usize = 0x1000;
const RING_BASES: [usize; 4] = [0x200, 0x400, 0x300, 0x500];
const ENDPOINTS: [u8; 4] = [0, 2, 3, 5];
const PORT_CHANGES: u32 = (1 << 17) | (1 << 19) | (1 << 21) | (1 << 22) | (1 << 23);
const XHCI_CTRL_IE: u32 = 1 << 4;
const XHCI_ST_IP: u32 = 1 << 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    DmaRegion,
    Timeout,
    RingFull,
    Transfer,
    UnsupportedSpeed,
}

#[derive(Default)]
pub struct Statistics {
    pub events: u64,
    pub setups: u64,
    pub last_event: [u32; 4],
    pub resets: u64,
    pub errors: u64,
    pub dropped: u64,
    pub received: u64,
    pub transmitted: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub control: u32,
    pub port: u32,
    pub port_halt: u32,
    pub vbus_id: u32,
    pub enqueue: u32,
    pub dequeue: u32,
    pub endpoint_halt: u32,
    pub endpoint_pause: u32,
    pub ep0_state: u32,
    pub configuration: u8,
    pub dtr: bool,
}

struct Ring {
    next: usize,
    cycle: u32,
    occupied: u16,
}
impl Ring {
    const fn new() -> Self {
        Self {
            next: 0,
            cycle: 1,
            occupied: 0,
        }
    }
}

#[derive(Clone, Copy)]
enum ControlPhase {
    In { zlp: bool },
    OutLine,
    Status,
    Zlp,
}

#[derive(Clone, Copy)]
struct Receive {
    cursor: usize,
    length: usize,
}

/// Hardware access, DMA storage and console transport are independent contracts.
pub struct Xudc<H, D> {
    hardware: H,
    dma: D,
    rings: [Ring; 4],
    acm: Acm,
    event: usize,
    event_cycle: u32,
    sequence: u16,
    control: Option<(u64, ControlPhase)>,
    tx: Option<(u64, usize)>,
    out: Option<u64>,
    rx: Option<Receive>,
    notify: Option<u64>,
    notification_needed: bool,
    zlp_needed: bool,
    high_speed: bool,
    initialized: bool,
    failed: bool,
    pub statistics: Statistics,
}

impl<H, D> Xudc<H, D> {
    pub const fn new(hardware: H, dma: D) -> Self {
        Self {
            hardware,
            dma,
            rings: [const { Ring::new() }; 4],
            acm: Acm::new(),
            event: 0,
            event_cycle: 1,
            sequence: 0,
            control: None,
            tx: None,
            out: None,
            rx: None,
            notify: None,
            notification_needed: false,
            zlp_needed: false,
            high_speed: false,
            initialized: false,
            failed: false,
            statistics: Statistics {
                events: 0,
                setups: 0,
                last_event: [0; 4],
                resets: 0,
                errors: 0,
                dropped: 0,
                received: 0,
                transmitted: 0,
            },
        }
    }
}

impl<H: Mmio + Clock, D: DmaBuffer> Xudc<H, D> {
    /// Read-only boot diagnostics. Call only after successful initialization.
    pub fn snapshot(&mut self) -> Snapshot {
        Snapshot {
            control: self.hardware.read32(DEV + 0x30),
            port: self.hardware.read32(DEV + 0x3c),
            port_halt: self.hardware.read32(DEV + 0x6c),
            vbus_id: self.hardware.read32(PAD + 0xc60),
            enqueue: self.hardware.read32(DEV + 0x28),
            dequeue: self.hardware.read32(DEV + 0x20),
            endpoint_halt: self.hardware.read32(DEV + 0x50),
            endpoint_pause: self.hardware.read32(DEV + 0x54),
            ep0_state: self.dma.read32(CONTEXT) & 7,
            configuration: self.acm.configuration,
            dtr: self.acm.dtr,
        }
    }
    fn update(&mut self, address: u64, clear: u32, set: u32) {
        let value = self.hardware.read32(address);
        self.hardware.write32(address, (value & !clear) | set);
    }
    fn wait(&mut self, address: u64, mask: u32, value: u32) -> Result<(), Error> {
        for _ in 0..1000 {
            if self.hardware.read32(address) & mask == value {
                return Ok(());
            }
            self.hardware.delay_us(1);
        }
        Err(Error::Timeout)
    }
    fn dma_address(&self, offset: usize) -> u64 {
        self.dma.physical_base() + offset as u64
    }
    fn put_bytes(&mut self, offset: usize, bytes: &[u8]) {
        for (index, chunk) in bytes.chunks(4).enumerate() {
            let mut word = [0; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            self.dma
                .write32(offset + index * 4, u32::from_le_bytes(word));
        }
    }
    fn get_bytes(&mut self, offset: usize, output: &mut [u8]) {
        for (index, byte) in output.iter_mut().enumerate() {
            let address = offset + index;
            let word = self.dma.read32(address & !3).to_le_bytes();
            *byte = word[address & 3];
        }
    }
    fn endpoint(&mut self, ring: usize) -> Result<(), Error> {
        let ep = ENDPOINTS[ring];
        self.rings[ring] = Ring::new();
        for offset in (RING_BASES[ring]..RING_BASES[ring] + 256).step_by(4) {
            self.dma.write32(offset, 0);
        }
        let base = self.dma_address(RING_BASES[ring]) as u32;
        self.dma.write32(RING_BASES[ring] + 240, base);
        self.dma.write32(RING_BASES[ring] + 252, (6 << 10) | 2);
        let context = CONTEXT + usize::from(ep) * 64;
        for offset in (context..context + 64).step_by(4) {
            self.dma.write32(offset, 0);
        }
        let packet = if ring == 0 {
            64
        } else if ring == 3 {
            16
        } else {
            self.packet_size()
        };
        let kind = [4, 2, 6, 7][ring];
        let interval = if ring == 3 {
            if self.high_speed { 8 } else { 7 }
        } else {
            0
        };
        self.dma.write32(context, 1 | (interval << 16));
        self.dma
            .write32(context + 4, 6 | (kind << 3) | (packet << 16));
        self.dma.write32(context + 8, base | 1);
        self.dma.write32(
            context + 16,
            if ring == 0 {
                8
            } else if ring == 3 {
                10 | (16 << 16)
            } else {
                512
            },
        );
        self.dma.write32(context + 24, 3 << 18); // NVIDIA consecutive error count.
        self.hardware.barrier();
        if ring != 0 {
            self.reload(ep)?;
        }
        Ok(())
    }
    fn reload(&mut self, ep: u8) -> Result<(), Error> {
        let mask = 1 << ep;
        self.hardware.write32(DEV + 0x58, mask);
        self.wait(DEV + 0x58, mask, 0)?;
        // Every reload path must resume fetching, including EP0 after bus reset.
        self.update(DEV + 0x54, mask, 0);
        self.halt(ep, false)
    }
    fn packet_size(&self) -> u32 {
        if self.high_speed { 512 } else { 64 }
    }
    fn queue(&mut self, ring: usize, words: [u32; 4]) -> Result<u64, Error> {
        let state = &mut self.rings[ring];
        if state.occupied & (1 << state.next) != 0 {
            return Err(Error::RingFull);
        }
        let offset = RING_BASES[ring] + state.next * 16;
        let pointer = self.dma.physical_base() + offset as u64;
        state.occupied |= 1 << state.next;
        for (index, word) in words[..3].iter().enumerate() {
            self.dma.write32(offset + index * 4, *word);
        }
        self.hardware.barrier();
        self.dma.write32(offset + 12, words[3] | state.cycle);
        state.next += 1;
        if state.next == 15 {
            self.dma
                .write32(RING_BASES[ring] + 252, (6 << 10) | 2 | state.cycle);
            state.next = 0;
            state.cycle ^= 1;
        }
        self.hardware.barrier();
        self.hardware.write32(
            DEV + 4,
            (u32::from(ENDPOINTS[ring]) << 8)
                | if ring == 0 {
                    u32::from(self.sequence) << 16
                } else {
                    0
                },
        );
        Ok(pointer)
    }
    fn complete(&mut self, ring: usize, pointer: u64) -> bool {
        let Some(offset) = pointer.checked_sub(self.dma_address(RING_BASES[ring])) else {
            return false;
        };
        if offset >= 240 || offset % 16 != 0 {
            return false;
        }
        let mask = 1 << (offset / 16);
        let occupied = self.rings[ring].occupied & mask != 0;
        self.rings[ring].occupied &= !mask;
        occupied
    }
    fn data(&mut self, length: usize, input: bool, phase: ControlPhase) -> Result<(), Error> {
        let pointer = self.queue(
            0,
            [
                self.dma_address(CONTROL) as u32,
                0,
                length as u32,
                (3 << 10) | (u32::from(input) << 16) | (1 << 2) | (1 << 5),
            ],
        )?;
        self.control = Some((pointer, phase));
        Ok(())
    }
    fn status(&mut self, input: bool) -> Result<(), Error> {
        let pointer = self.queue(
            0,
            [0, 0, 0, (4 << 10) | (u32::from(input) << 16) | (1 << 5)],
        )?;
        self.control = Some((pointer, ControlPhase::Status));
        Ok(())
    }
    fn halt(&mut self, ep: u8, halted: bool) -> Result<(), Error> {
        if (self.hardware.read32(DEV + 0x50) & (1 << ep) != 0) == halted {
            return Ok(());
        }
        self.update(
            DEV + 0x50,
            if halted { 0 } else { 1 << ep },
            if halted { 1 << ep } else { 0 },
        );
        self.wait(DEV + 0x5c, 1 << ep, 1 << ep)?;
        self.hardware.write32(DEV + 0x5c, 1 << ep);
        Ok(())
    }
    fn stop_data(&mut self) -> Result<(), Error> {
        for &ep in ENDPOINTS.iter().skip(1) {
            if self.acm.configuration != 0 {
                self.update(DEV + 0x50, 0, 1 << ep);
                self.dma.write32(CONTEXT + usize::from(ep) * 64, 0);
                self.hardware.barrier();
                self.wait(DEV + 0x5c, 1 << ep, 1 << ep)?;
                self.hardware.write32(DEV + 0x5c, 1 << ep);
            }
            self.dma.write32(CONTEXT + usize::from(ep) * 64, 0);
        }
        if let Some((_, length)) = self.tx.take() {
            self.statistics.dropped = self.statistics.dropped.saturating_add(length as u64);
        }
        self.out = None;
        self.rx = None;
        self.notify = None;
        self.zlp_needed = false;
        self.acm.configuration = 0;
        self.acm.dtr = false;
        self.update(DEV + 0x30, 1, 0);
        Ok(())
    }
    fn setup(&mut self, words: [u32; 4]) -> Result<(), Error> {
        self.sequence = words[2] as u16;
        self.control = None; // A new SETUP supersedes the previous control request.
        self.halt(0, false)?;
        let setup = Setup::from_words([words[0], words[1]]);
        let old_dtr = self.acm.dtr;
        let mut output = [0; CONTROL_SIZE];
        match self.acm.setup(setup, &mut output, self.high_speed) {
            Reply::Data(length) => {
                self.put_bytes(CONTROL, &output[..length]);
                self.data(
                    length,
                    true,
                    ControlPhase::In {
                        zlp: length != 0 && length % 64 == 0 && length < usize::from(setup.length),
                    },
                )
            }
            Reply::LineCoding => self.data(7, false, ControlPhase::OutLine),
            Reply::Address(address) => {
                self.update(DEV + 0x30, 0x7f00_0000, u32::from(address) << 24);
                self.dma.write32(CONTEXT + 44, u32::from(address));
                self.status(true)
            }
            Reply::Configuration(configuration) => {
                self.stop_data()?;
                if configuration != 0 {
                    for ring in 1..4 {
                        self.endpoint(ring)?;
                    }
                    self.update(DEV + 0x30, 0, 1);
                    self.acm.configuration = 1;
                    self.notification_needed = true;
                }
                self.status(true)
            }
            Reply::Halt { endpoint, halted } => {
                self.halt(endpoint, true)?;
                if !halted {
                    let ring = ENDPOINTS
                        .iter()
                        .position(|&ep| ep == endpoint)
                        .ok_or(Error::Transfer)?;
                    if ring == 2 {
                        if let Some((_, length)) = self.tx.take() {
                            self.statistics.dropped += length as u64;
                        }
                        self.zlp_needed = false;
                    }
                    if ring == 1 {
                        self.out = None;
                        self.rx = None;
                    }
                    if ring == 3 {
                        self.notify = None;
                    }
                    self.endpoint(ring)?;
                }
                self.status(true)
            }
            Reply::Status => {
                self.notification_needed |= old_dtr != self.acm.dtr;
                self.status(true)
            }
            Reply::Stall => self.halt(0, true),
        }
    }
    fn transfer(&mut self, words: [u32; 4]) -> Result<(), Error> {
        let ep = ((words[3] >> 16) & 31) as u8;
        let Some(ring) = ENDPOINTS.iter().position(|&id| id == ep) else {
            return Ok(());
        };
        let pointer = u64::from(words[0]) | (u64::from(words[1]) << 32);
        if !self.complete(ring, pointer) {
            return Ok(());
        }
        let code = words[2] >> 24;
        let remaining = (words[2] & 0x00ff_ffff) as usize;
        if !matches!(code, 1 | 13) {
            // Control sequence errors are normal when the host aborts a request.
            if ring == 0 && code == 223 {
                if self.control.is_some_and(|(p, _)| p == pointer) {
                    self.control = None;
                }
                return Ok(());
            }
            return Err(Error::Transfer);
        }
        match ring {
            0 => {
                let Some((expected, phase)) = self.control else {
                    return Ok(());
                };
                if pointer != expected {
                    return Ok(());
                }
                self.control = None;
                match phase {
                    ControlPhase::In { zlp: true } => self.data(0, true, ControlPhase::Zlp),
                    ControlPhase::In { zlp: false } | ControlPhase::Zlp => self.status(false),
                    ControlPhase::OutLine => {
                        if remaining != 0 {
                            return self.halt(0, true);
                        }
                        let first = self.dma.read32(CONTROL).to_le_bytes();
                        let second = self.dma.read32(CONTROL + 4).to_le_bytes();
                        self.acm.line_coding[..4].copy_from_slice(&first);
                        self.acm.line_coding[4..].copy_from_slice(&second[..3]);
                        self.status(true)
                    }
                    ControlPhase::Status => Ok(()),
                }
            }
            1 => {
                if self.out != Some(pointer) {
                    return Ok(());
                }
                self.out = None;
                if remaining > 512 {
                    return Err(Error::Transfer);
                }
                let length = 512 - remaining;
                self.statistics.received += length as u64;
                self.rx = (length != 0).then_some(Receive { cursor: 0, length });
                Ok(())
            }
            2 => {
                if let Some((expected, length)) = self.tx {
                    if pointer == expected {
                        self.tx = None;
                        if remaining > length {
                            return Err(Error::Transfer);
                        }
                        self.statistics.transmitted += (length - remaining) as u64;
                        self.statistics.dropped += remaining as u64;
                        if remaining != 0 {
                            return Err(Error::Transfer);
                        }
                    }
                }
                Ok(())
            }
            _ => {
                if self.notify == Some(pointer) {
                    self.notify = None;
                }
                Ok(())
            }
        }
    }
    fn port(&mut self) -> Result<(), Error> {
        let status = self.hardware.read32(DEV + 0x3c);
        self.update(DEV + 0x6c, 1, 0);
        // PORTSC change bits are W1C; preserve read-only fields but not reset strobes.
        self.hardware
            .write32(DEV + 0x3c, status & !((1 << 4) | (1 << 30)));
        if status & PORT_CHANGES != 0 {
            let speed = (status >> 10) & 15;
            if status & 1 != 0 && !matches!(speed, 1 | 3) {
                return Err(Error::UnsupportedSpeed);
            }
            self.high_speed = speed == 3;
        }
        if status & 1 == 0 || status & ((1 << 19) | (1 << 21)) != 0 {
            self.stop_data()?;
            self.acm.reset();
            self.update(DEV + 0x30, 0x7f00_0000, 0);
            self.control = None;
            self.endpoint(0)?;
            self.reload(0)?;
            self.failed = false;
            self.statistics.resets += 1;
        }
        Ok(())
    }
    fn arm_data(&mut self) -> Result<(), Error> {
        if self.acm.configuration == 0 || self.failed {
            return Ok(());
        }
        if self.out.is_none()
            && self.rx.is_none()
            && self.hardware.read32(DEV + 0x50) & (1 << 2) == 0
        {
            self.out = Some(self.queue(
                1,
                [
                    self.dma_address(RX_BUFFER) as u32,
                    0,
                    512,
                    (1 << 10) | (1 << 2) | (1 << 5),
                ],
            )?);
        }
        if self.notification_needed
            && self.notify.is_none()
            && self.hardware.read32(DEV + 0x50) & (1 << 5) == 0
        {
            self.put_bytes(
                NOTIFY_BUFFER,
                &[
                    0xa1,
                    0x20,
                    0,
                    0,
                    0,
                    0,
                    2,
                    0,
                    if self.acm.dtr { 3 } else { 0 },
                    0,
                ],
            );
            self.notify = Some(self.queue(
                3,
                [
                    self.dma_address(NOTIFY_BUFFER) as u32,
                    0,
                    10,
                    (1 << 10) | (1 << 5),
                ],
            )?);
            self.notification_needed = false;
        }
        if self.zlp_needed && self.tx.is_none() {
            self.tx = Some((
                self.queue(
                    2,
                    [
                        self.dma_address(TX_BUFFER) as u32,
                        0,
                        0,
                        (1 << 10) | (1 << 5),
                    ],
                )?,
                0,
            ));
            self.zlp_needed = false;
        }
        Ok(())
    }
}

impl<H: Mmio + Clock, D: DmaBuffer> Driver for Xudc<H, D> {
    type Error = Error;
    fn initialize(&mut self) -> Result<(), Error> {
        let base = self.dma.physical_base();
        if base % 4096 != 0
            || self.dma.size() < DMA_SIZE
            || base < RESIDENT_BASE
            || base
                .checked_add(DMA_SIZE as u64)
                .is_none_or(|end| end > RESIDENT_BASE + RESIDENT_SIZE || end > 1 << 32)
        {
            return Err(Error::DmaRegion);
        }
        self.initialized = false;
        self.failed = false;
        self.acm.reset();
        self.control = None;
        self.tx = None;
        self.out = None;
        self.rx = None;
        self.notify = None;
        self.zlp_needed = false;
        self.notification_needed = false;
        // Keep this USB client in bypass even if the guest later enables the SMMU.
        self.hardware.write32(DEV_ASID, 0);
        self.hardware.barrier();
        for partition in [20, 21] {
            if self.hardware.read32(PMC + 0x38) & (1 << partition) == 0 {
                self.hardware.write32(PMC + 0x30, 0x100 | partition);
                self.wait(PMC + 0x38, 1 << partition, 1 << partition)?;
                self.hardware.write32(PMC + 0x34, 1 << partition);
            }
        }
        self.hardware.write32(CAR + 0x300, 1 << 22);
        self.hardware.write32(CAR + 0x324, 1 << 22);
        self.hardware.write32(CAR + 0x448, 1 << 15);
        self.hardware.write32(CAR + 0x438, 1 << 15);
        self.hardware.delay_us(2);
        self.hardware.write32(CAR + 0x43c, (1 << 15) | (1 << 14));
        self.hardware.delay_us(2);
        self.update(PAD + 4, (3 << 18) | 3, (1 << 18) | 1);
        self.update(CAR + 0xcc, 0, 1 << 29);
        let pllu = (self.hardware.read32(CAR + 0xc0) & 0xffe0_0000)
            | (1 << 24)
            | (1 << 16)
            | (25 << 8)
            | 2;
        self.hardware.write32(CAR + 0xc0, pllu);
        self.hardware.write32(CAR + 0xc0, pllu | (1 << 30));
        self.wait(CAR + 0xc0, 1 << 27, 1 << 27)?;
        self.hardware.delay_us(8);
        self.update(CAR + 0xc0, 0, 0x02e0_0000);
        self.update(CAR + 0x52c, 3, 1);
        self.update(CAR + 0x480, !0xff00_00ff, (25 << 16) | (1 << 8));
        self.update(CAR + 0x488, !0xff00_003f, 24 << 18);
        self.update(CAR + 0x484, !0x07ff_a000, (1 << 15) | 375);
        self.wait(CAR + 0x52c, 1 << 31, 1 << 31)?;
        self.update(CAR + 0x488, !0xfeff_ffe8, 0x0200_002a);
        self.hardware.delay_us(2);
        let calibration = self.hardware.read32(0x7000_f9f0);
        let extension = self.hardware.read32(0x7000_fb50);
        self.update(PAD + 0x88, 0x3f, calibration & 0x3f);
        self.update(
            PAD + 0x8c,
            !0x83ff_ff87,
            ((calibration & 0x780) >> 4) | ((extension & 31) << 26),
        );
        self.update(PAD + 0x84, 0x1c0, 0x80);
        self.update(PAD + 0x88, !0xdbff_ffff, 0);
        self.update(PAD + 0x8c, 4, 0);
        self.update(PAD + 0x80, 1, 0);
        self.update(PAD + 0x284, 1 << 11, 0);
        self.hardware.read32(PAD + 0x8c);
        self.hardware.write32(CAR + 0x29c, 1 << 18);
        self.update(CAR + 0x6cc, 0xff, 6);
        self.hardware.write32(PAD + 0x288, 0x0451_e000);
        self.hardware.write32(PAD + 0x288, 0x0051_e000);
        self.hardware.delay_us(100);
        self.hardware.write32(PAD + 0x288, 0x0451_e000);
        self.hardware.delay_us(3);
        self.hardware.write32(PAD + 0x288, 0x0051_e000);
        self.hardware.delay_us(100);
        self.update(PAD + 0x288, 0, 1 << 26);
        self.hardware.write32(CAR + 0x2a0, 1 << 18);
        self.hardware.delay_us(30);
        self.update(PAD + 8, 3, 2);
        self.update(PAD + 0x14, 15, 0);
        self.update(PMC + 0xf0, 12, 0);
        self.hardware.delay_us(1);
        self.update(CAR + 0xc4, 0, 1);
        self.hardware.delay_us(2);
        self.hardware.write32(CAR + 0x330, 1 << 31);
        self.update(CAR + 0x60c, !0x1fff_ff00, (1 << 29) | 6);
        self.hardware.delay_us(2);
        self.update(CAR + 0x608, !0x1fff_ffff, 2 << 29);
        self.hardware.write32(CAR + 0x448, 1 << 28);
        self.update(CAR + 0x610, !0x1fff_ff00, (3 << 29) | 6);
        self.hardware.write32(CAR + 0x43c, 1 << 28);
        self.hardware.write32(CAR + 0x314, 1 << 31);
        self.hardware.delay_us(2);
        self.update(DEV + 0x9180, 0, 1);
        self.update(DEV + 0x8004, 0, 7);
        self.hardware.delay_us(1);
        self.hardware.write32(DEV + 0x8010, DEV as u32);
        self.update(DEV + 0x9188, 1 << 16, 0);
        self.hardware.write32(DEV + 0x30, 0);
        for offset in (0..DMA_SIZE).step_by(4) {
            self.dma.write32(offset, 0);
        }
        self.event = 0;
        self.event_cycle = 1;
        for (low, offset) in [(0x10, EVENT0), (0x18, EVENT1)] {
            self.hardware
                .write32(DEV + low, self.dma_address(offset) as u32);
            self.hardware.write32(DEV + low + 4, 0);
        }
        self.hardware.write32(DEV + 8, (16 << 16) | 16);
        let event_base = self.dma_address(EVENT0) as u32;
        self.hardware.write32(DEV + 0x28, event_base | 1);
        self.hardware.write32(DEV + 0x2c, 0);
        self.hardware.write32(DEV + 0x20, event_base);
        self.hardware.write32(DEV + 0x24, 0);
        self.endpoint(0)?;
        self.hardware
            .write32(DEV + 0x40, self.dma_address(CONTEXT) as u32);
        self.hardware.write32(DEV + 0x44, 0);
        self.hardware.write32(DEV + 0x38, 0);
        self.hardware.write32(PAD + 0x20, 0);
        self.hardware.write32(PAD + 0x24, 0);
        self.update(PAD + 0xc60, (3 << 12) | (3 << 16), (1 << 12) | (1 << 16));
        self.update(DEV + 0x6c, 1, 0);
        self.hardware.write32(DEV + 0x34, XHCI_ST_IP);
        self.hardware.barrier();
        self.hardware
            .write32(DEV + 0x30, (1 << 31) | XHCI_CTRL_IE | 2);
        self.update(DEV + 0x85c, 3, 2);
        self.update(DEV + 0x3c, 15 << 5, (1 << 16) | (5 << 5));
        self.update(DEV + 0x85c, 3, 0);
        // Hekate explicitly asserts software VBUS as well as a floating ID.
        // Selecting the VBUS override alone leaves its value off after reset.
        self.update(PAD + 0xc60, 15 << 18, (8 << 18) | VBUS_ON);
        self.initialized = true;
        Ok(())
    }

    fn poll(&mut self) -> Result<(), Error> {
        if !self.initialized {
            return Ok(());
        }
        if self.hardware.read32(DEV + 0x34) & XHCI_ST_IP != 0 {
            self.hardware.write32(DEV + 0x34, XHCI_ST_IP);
        }
        for _ in 0..32 {
            let offset = self.event * 16;
            let control = self.dma.read32(offset + 12);
            if control & 1 != self.event_cycle {
                break;
            }
            self.hardware.barrier();
            let words = [
                self.dma.read32(offset),
                self.dma.read32(offset + 4),
                self.dma.read32(offset + 8),
                control,
            ];
            self.statistics.events += 1;
            self.statistics.last_event = words;
            if (control >> 10) & 63 == 63 {
                self.statistics.setups += 1;
            }
            let result = match (control >> 10) & 63 {
                32 => self.transfer(words),
                34 => self.port(),
                63 => self.setup(words),
                _ => Err(Error::Transfer),
            };
            self.event += 1;
            if self.event == 32 {
                self.event = 0;
                self.event_cycle ^= 1;
            }
            self.hardware.barrier();
            self.hardware.write32(
                DEV + 0x20,
                self.dma_address(self.event * 16) as u32 | (1 << 3),
            );
            if let Err(error) = result {
                self.statistics.errors += 1;
                self.failed = true;
                return Err(error);
            }
        }
        self.arm_data()
    }
}

impl<H: Mmio + Clock, D: DmaBuffer> RxTransport for Xudc<H, D> {
    fn receive(&mut self, output: &mut [u8]) -> Result<usize, Error> {
        let Some(mut receive) = self.rx else {
            return Ok(0);
        };
        let count = output.len().min(receive.length - receive.cursor);
        self.get_bytes(RX_BUFFER + receive.cursor, &mut output[..count]);
        receive.cursor += count;
        if receive.cursor == receive.length {
            self.rx = None;
            self.arm_data()?;
        } else {
            self.rx = Some(receive);
        }
        Ok(count)
    }
}

impl<H: Mmio + Clock, D: DmaBuffer> TxTransport for Xudc<H, D> {
    fn connected(&self) -> bool {
        self.initialized && !self.failed && self.acm.configuration == 1 && self.acm.dtr
    }
    fn send_capacity(&self) -> usize {
        if self.connected() && self.tx.is_none() && !self.zlp_needed {
            512
        } else {
            0
        }
    }
    fn send(&mut self, bytes: &[u8]) -> Result<usize, Error> {
        let capacity = self.send_capacity();
        if capacity == 0 || bytes.is_empty() || self.hardware.read32(DEV + 0x50) & (1 << 3) != 0 {
            return Ok(0);
        }
        let length = bytes.len().min(capacity);
        self.put_bytes(TX_BUFFER, &bytes[..length]);
        let pointer = self.queue(
            2,
            [
                self.dma_address(TX_BUFFER) as u32,
                0,
                length as u32,
                (1 << 10) | (1 << 5),
            ],
        )?;
        self.tx = Some((pointer, length));
        self.zlp_needed = length % self.packet_size() as usize == 0;
        Ok(length)
    }
}
