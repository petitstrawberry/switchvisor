//! CDC-NCM 1.0 NTB16, no CRC or segmentation offload.
use crate::net::FRAME_SIZE;

pub const NTB_SIZE: usize = 16 * 1024;
pub const MAX_DATAGRAMS: usize = 16;
// Remainder zero aligns the Ethernet *payload*, not the Ethernet header.
// NTH + NDP occupy 28 bytes; two pad bytes align (offset + ETH_HLEN) to 4.
pub const FRAME_OFFSET: usize = 30;
pub const CONTROL_INTERFACE: u16 = 5;
pub const DATA_INTERFACE: u16 = 6;
pub const DESCRIPTOR_SIZE: usize = 85;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Header,
    Pointer,
    Length,
    Overlap,
    TooMany,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Datagram {
    pub offset: usize,
    pub length: usize,
}

pub struct Block {
    pub datagrams: [Datagram; MAX_DATAGRAMS],
    pub count: usize,
}

impl Block {
    pub const fn empty() -> Self {
        Self {
            datagrams: [Datagram {
                offset: 0,
                length: 0,
            }; MAX_DATAGRAMS],
            count: 0,
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Result<usize, Error> {
    let pair = bytes.get(at..at + 2).ok_or(Error::Length)?;
    Ok(usize::from(u16::from_le_bytes([pair[0], pair[1]])))
}
fn overlaps(a: Datagram, b: Datagram) -> bool {
    a.offset < b.offset + b.length && b.offset < a.offset + a.length
}

/// Validate the entire NTB before exposing any frame. NDP chains, terminators,
/// datagram bounds and overlap with *all* metadata are checked with fixed bounds.
pub fn decode(bytes: &[u8]) -> Result<Block, Error> {
    if bytes.len() < 12
        || bytes.len() > NTB_SIZE
        || &bytes[..4] != b"NCMH"
        || u16_at(bytes, 4)? != 12
        || u16_at(bytes, 8)? != bytes.len()
    {
        return Err(Error::Header);
    }
    let mut block = Block::empty();
    let mut tables = [Datagram {
        offset: 0,
        length: 0,
    }; MAX_DATAGRAMS];
    let mut table_count = 0;
    let mut next = u16_at(bytes, 10)?;
    if next == 0 {
        return Err(Error::Pointer);
    }
    while next != 0 {
        if table_count == MAX_DATAGRAMS {
            return Err(Error::TooMany);
        }
        if next < 12 || next % 4 != 0 || bytes.get(next..next + 4) != Some(b"NCM0") {
            return Err(Error::Pointer);
        }
        let size = u16_at(bytes, next + 4)?;
        if size < 12 || size % 4 != 0 || next + size > bytes.len() {
            return Err(Error::Length);
        }
        let table = Datagram {
            offset: next,
            length: size,
        };
        if tables[..table_count]
            .iter()
            .any(|&old| overlaps(old, table))
        {
            return Err(Error::Overlap);
        }
        tables[table_count] = table;
        table_count += 1;
        let mut terminated = false;
        for entry in (next + 8..next + size).step_by(4) {
            let offset = u16_at(bytes, entry)?;
            let length = u16_at(bytes, entry + 2)?;
            if offset == 0 && length == 0 {
                terminated = true;
                break;
            }
            if offset < 12
                || (offset + 14) % 4 != 0
                || !(14..=FRAME_SIZE).contains(&length)
                || offset + length > bytes.len()
            {
                return Err(Error::Length);
            }
            if block.count == MAX_DATAGRAMS {
                return Err(Error::TooMany);
            }
            let datagram = Datagram { offset, length };
            if block.datagrams[..block.count]
                .iter()
                .any(|&old| overlaps(old, datagram))
            {
                return Err(Error::Overlap);
            }
            block.datagrams[block.count] = datagram;
            block.count += 1;
        }
        if !terminated {
            return Err(Error::Pointer);
        }
        next = u16_at(bytes, next + 6)?;
    }
    for datagram in &block.datagrams[..block.count] {
        if tables[..table_count]
            .iter()
            .any(|&table| overlaps(table, *datagram))
        {
            return Err(Error::Overlap);
        }
    }
    Ok(block)
}

pub fn encode(frame: &[u8], sequence: u16, output: &mut [u8]) -> Result<usize, Error> {
    let length = FRAME_OFFSET + frame.len();
    if !(14..=FRAME_SIZE).contains(&frame.len()) || output.len() < length {
        return Err(Error::Length);
    }
    output[..FRAME_OFFSET].fill(0);
    output[..4].copy_from_slice(b"NCMH");
    output[4..6].copy_from_slice(&12u16.to_le_bytes());
    output[6..8].copy_from_slice(&sequence.to_le_bytes());
    output[8..10].copy_from_slice(&(length as u16).to_le_bytes());
    output[10..12].copy_from_slice(&12u16.to_le_bytes());
    output[12..16].copy_from_slice(b"NCM0");
    output[16..18].copy_from_slice(&16u16.to_le_bytes());
    output[20..22].copy_from_slice(&(FRAME_OFFSET as u16).to_le_bytes());
    output[22..24].copy_from_slice(&(frame.len() as u16).to_le_bytes());
    output[FRAME_OFFSET..length].copy_from_slice(frame);
    Ok(length)
}

/// Build a multi-datagram NTB in its final DMA slot. The NDP follows the data,
/// so a new frame never moves previously copied payload or reserves MTU-sized
/// holes. Failed appends leave both metadata and the output unchanged.
pub struct Encoder {
    block: Block,
    end: usize,
}
impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}
impl Encoder {
    pub const fn new() -> Self {
        Self {
            block: Block::empty(),
            end: 12,
        }
    }
    pub fn push(&mut self, frame: &[u8], output: &mut [u8]) -> Result<(), Error> {
        if self.block.count == MAX_DATAGRAMS {
            return Err(Error::TooMany);
        }
        if !(14..=FRAME_SIZE).contains(&frame.len()) {
            return Err(Error::Length);
        }
        let offset = ((self.end + 14 + 3) & !3) - 14;
        let end = offset + frame.len();
        let ndp = (end + 3) & !3;
        let total = ndp + 8 + (self.block.count + 2) * 4;
        if total > output.len().min(NTB_SIZE) {
            return Err(Error::Length);
        }
        output[self.end..offset].fill(0);
        output[offset..end].copy_from_slice(frame);
        self.block.datagrams[self.block.count] = Datagram {
            offset,
            length: frame.len(),
        };
        self.block.count += 1;
        self.end = end;
        Ok(())
    }
    pub fn finish(self, sequence: u16, output: &mut [u8]) -> Result<usize, Error> {
        let ndp = (self.end + 3) & !3;
        let size = 8 + (self.block.count + 1) * 4;
        let length = ndp + size;
        if self.block.count == 0 || length > output.len().min(NTB_SIZE) {
            return Err(Error::Length);
        }
        output[..12].copy_from_slice(&[
            b'N',
            b'C',
            b'M',
            b'H',
            12,
            0,
            sequence as u8,
            (sequence >> 8) as u8,
            length as u8,
            (length >> 8) as u8,
            ndp as u8,
            (ndp >> 8) as u8,
        ]);
        output[self.end..length].fill(0);
        output[ndp..ndp + 4].copy_from_slice(b"NCM0");
        output[ndp + 4..ndp + 6].copy_from_slice(&(size as u16).to_le_bytes());
        for (index, frame) in self.block.datagrams[..self.block.count].iter().enumerate() {
            let at = ndp + 8 + index * 4;
            output[at..at + 2].copy_from_slice(&(frame.offset as u16).to_le_bytes());
            output[at + 2..at + 4].copy_from_slice(&(frame.length as u16).to_le_bytes());
        }
        Ok(length)
    }
}

pub struct Function {
    pub enabled: bool,
    pub alternate: u8,
    pub packet_filter: u16,
    pub input_size: usize,
}
impl Function {
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            alternate: 0,
            packet_filter: 0,
            input_size: NTB_SIZE,
        }
    }
    pub fn active(&self) -> bool {
        self.enabled && self.alternate == 1
    }
    pub fn parameters(&self) -> [u8; 28] {
        let mut out = [0; 28];
        out[..2].copy_from_slice(&28u16.to_le_bytes());
        out[2..4].copy_from_slice(&1u16.to_le_bytes()); // NTB16 only
        out[4..8].copy_from_slice(&(NTB_SIZE as u32).to_le_bytes());
        out[8..10].copy_from_slice(&4u16.to_le_bytes());
        out[12..14].copy_from_slice(&4u16.to_le_bytes());
        out[16..20].copy_from_slice(&(NTB_SIZE as u32).to_le_bytes());
        out[20..22].copy_from_slice(&4u16.to_le_bytes());
        out[24..26].copy_from_slice(&4u16.to_le_bytes());
        out[26..28].copy_from_slice(&(MAX_DATAGRAMS as u16).to_le_bytes());
        out
    }
}
