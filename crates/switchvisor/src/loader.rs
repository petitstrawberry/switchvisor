//! Binary protocol and state machine for pre-guest guest-RAM bundle uploads.

use crate::{
    memory::AddressRange,
    payload::{Crc32, RESIDENT_BASE, STACK_TOP},
};

pub const MAGIC: [u8; 4] = *b"SWVL";
pub const VERSION: u16 = 2;
pub const HEADER_SIZE: usize = 32;
pub const MAX_MESSAGE_SIZE: usize = 4096;
pub const MAX_BODY_SIZE: usize = MAX_MESSAGE_SIZE - HEADER_SIZE;
pub const BUNDLE_DESCRIPTOR_SIZE: usize = 80;
pub const IMAGE_DESCRIPTOR_SIZE: usize = 32;
pub const MAX_IMAGES: usize = 16;
pub const MAX_RESPONSE_SIZE: usize = HEADER_SIZE + 48;
pub const REPLY_FLAG: u16 = 0x8000;
pub const PRESERVE_BOOT_ARGS: u32 = 1;

/// The loader accepts arbitrary non-overlapping ranges in low guest RAM.
pub const GUEST_RAM_BASE: u64 = 0x8000_0000;
pub const GUEST_RAM_END: u64 = RESIDENT_BASE;
const BOOT_STACK_SIZE: u64 = 64 * 1024;
const FRAMEBUFFER_BASE: u64 = 0xf5a0_0000;
const FRAMEBUFFER_SIZE: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Opcode {
    Hello = 1,
    BeginBundle = 2,
    BeginImage = 3,
    Data = 4,
    EndImage = 5,
    CommitBundle = 6,
    Abort = 7,
    Boot = 8,
    Status = 9,
}

impl Opcode {
    fn decode(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::BeginBundle,
            3 => Self::BeginImage,
            4 => Self::Data,
            5 => Self::EndImage,
            6 => Self::CommitBundle,
            7 => Self::Abort,
            8 => Self::Boot,
            9 => Self::Status,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum State {
    Idle = 0,
    Bundle = 1,
    Receiving = 2,
    Ready = 3,
    Disabled = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum StatusCode {
    Ok = 0,
    UnknownOpcode = 1,
    BadLength = 2,
    BadState = 3,
    InvalidDescriptor = 4,
    BadOffset = 5,
    Checksum = 6,
    Storage = 7,
    GuestRunning = 8,
    ImageCount = 9,
    Overlap = 10,
    Entry = 11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    Truncated,
    Magic,
    Version,
    Length,
    Reply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub opcode: u16,
    pub request_id: u32,
    pub length: u32,
    pub arg0: u64,
    pub arg1: u64,
}

impl Header {
    pub const fn request(opcode: Opcode, request_id: u32, length: u32) -> Self {
        Self {
            opcode: opcode as u16,
            request_id,
            length,
            arg0: 0,
            arg1: 0,
        }
    }

    pub fn encode(self, output: &mut [u8; HEADER_SIZE]) {
        output[..4].copy_from_slice(&MAGIC);
        output[4..6].copy_from_slice(&VERSION.to_le_bytes());
        output[6..8].copy_from_slice(&self.opcode.to_le_bytes());
        output[8..12].copy_from_slice(&self.request_id.to_le_bytes());
        output[12..16].copy_from_slice(&self.length.to_le_bytes());
        output[16..24].copy_from_slice(&self.arg0.to_le_bytes());
        output[24..32].copy_from_slice(&self.arg1.to_le_bytes());
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ProtocolError::Truncated);
        }
        if bytes[..4] != MAGIC {
            return Err(ProtocolError::Magic);
        }
        if u16_at(bytes, 4)? != VERSION {
            return Err(ProtocolError::Version);
        }
        let header = Self {
            opcode: u16_at(bytes, 6)?,
            request_id: u32_at(bytes, 8)?,
            length: u32_at(bytes, 12)?,
            arg0: u64_at(bytes, 16)?,
            arg1: u64_at(bytes, 24)?,
        };
        if header.length as usize > MAX_BODY_SIZE
            || bytes.len() != HEADER_SIZE + header.length as usize
        {
            return Err(ProtocolError::Length);
        }
        Ok(header)
    }
}

/// Entry state for a committed bundle. Image meaning remains guest-specific.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundleDescriptor {
    pub entry: u64,
    pub image_count: u32,
    pub flags: u32,
    pub registers: [u64; 8],
}

impl BundleDescriptor {
    pub fn preserve_boot_args(self) -> bool {
        self.flags & PRESERVE_BOOT_ARGS != 0
    }

    pub fn validate(self) -> bool {
        self.image_count != 0
            && self.image_count as usize <= MAX_IMAGES
            && self.entry % 4 == 0
            && guest_range(self.entry, 4)
            && self.flags & !PRESERVE_BOOT_ARGS == 0
            && (!self.preserve_boot_args() || self.registers.iter().all(|value| *value == 0))
    }

    pub fn encode(self, output: &mut [u8; BUNDLE_DESCRIPTOR_SIZE]) {
        output[..8].copy_from_slice(&self.entry.to_le_bytes());
        output[8..12].copy_from_slice(&self.image_count.to_le_bytes());
        output[12..16].copy_from_slice(&self.flags.to_le_bytes());
        for (index, value) in self.registers.iter().enumerate() {
            output[16 + index * 8..24 + index * 8].copy_from_slice(&value.to_le_bytes());
        }
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != BUNDLE_DESCRIPTOR_SIZE {
            return None;
        }
        let mut registers = [0; 8];
        for (index, value) in registers.iter_mut().enumerate() {
            *value = u64_at(bytes, 16 + index * 8).ok()?;
        }
        let descriptor = Self {
            entry: u64_at(bytes, 0).ok()?,
            image_count: u32_at(bytes, 8).ok()?,
            flags: u32_at(bytes, 12).ok()?,
            registers,
        };
        descriptor.validate().then_some(descriptor)
    }
}

/// One opaque file and its zero-filled runtime extent in guest RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageDescriptor {
    pub address: u64,
    pub file_size: u64,
    pub runtime_size: u64,
    pub crc32: u32,
    pub flags: u32,
}

impl ImageDescriptor {
    pub fn validate(self) -> bool {
        self.file_size != 0
            && self.runtime_size >= self.file_size
            && self.flags == 0
            && guest_range(self.address, self.runtime_size)
    }

    pub fn contains_entry(self, entry: u64) -> bool {
        entry % 4 == 0
            && self.address.checked_add(self.file_size).is_some_and(|end| {
                entry >= self.address && entry.checked_add(4).is_some_and(|value| value <= end)
            })
    }

    fn overlaps(self, other: Self) -> bool {
        let left = AddressRange::new(self.address, self.runtime_size).expect("validated image");
        let right = AddressRange::new(other.address, other.runtime_size).expect("validated image");
        left.overlaps(right)
    }

    pub fn encode(self, output: &mut [u8; IMAGE_DESCRIPTOR_SIZE]) {
        output[..8].copy_from_slice(&self.address.to_le_bytes());
        output[8..16].copy_from_slice(&self.file_size.to_le_bytes());
        output[16..24].copy_from_slice(&self.runtime_size.to_le_bytes());
        output[24..28].copy_from_slice(&self.crc32.to_le_bytes());
        output[28..32].copy_from_slice(&self.flags.to_le_bytes());
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != IMAGE_DESCRIPTOR_SIZE {
            return None;
        }
        let descriptor = Self {
            address: u64_at(bytes, 0).ok()?,
            file_size: u64_at(bytes, 8).ok()?,
            runtime_size: u64_at(bytes, 16).ok()?,
            crc32: u32_at(bytes, 24).ok()?,
            flags: u32_at(bytes, 28).ok()?,
        };
        descriptor.validate().then_some(descriptor)
    }
}

/// Reject ranges that could overwrite EL2's active boot stack or framebuffer.
pub fn guest_range(address: u64, size: u64) -> bool {
    let Ok(range) = AddressRange::new(address, size) else {
        return false;
    };
    if range.start() < GUEST_RAM_BASE || range.end() > GUEST_RAM_END {
        return false;
    }
    for protected in [
        AddressRange::new(STACK_TOP - BOOT_STACK_SIZE, BOOT_STACK_SIZE).unwrap(),
        AddressRange::new(FRAMEBUFFER_BASE, FRAMEBUFFER_SIZE).unwrap(),
    ] {
        if range.overlaps(protected) {
            return false;
        }
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageError;

pub trait Storage {
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), StorageError>;
    fn zero(&mut self, address: u64, length: u64) -> Result<(), StorageError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Boot(BundleDescriptor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response {
    pub header: Header,
    body: [u8; 48],
}

impl Response {
    fn new(request: Header, code: StatusCode, state: State, body: &[u8]) -> Self {
        let mut response = Self {
            header: Header {
                opcode: request.opcode | REPLY_FLAG,
                request_id: request.request_id,
                length: body.len() as u32,
                arg0: code as u64,
                arg1: state as u64,
            },
            body: [0; 48],
        };
        response.body[..body.len()].copy_from_slice(body);
        response
    }

    pub fn status_code(self) -> u64 {
        self.header.arg0
    }

    pub fn body(&self) -> &[u8] {
        &self.body[..self.header.length as usize]
    }

    pub fn encode(self, output: &mut [u8; MAX_RESPONSE_SIZE]) -> usize {
        let mut header = [0; HEADER_SIZE];
        self.header.encode(&mut header);
        output[..HEADER_SIZE].copy_from_slice(&header);
        let length = self.header.length as usize;
        output[HEADER_SIZE..HEADER_SIZE + length].copy_from_slice(&self.body[..length]);
        HEADER_SIZE + length
    }
}

pub struct Loader {
    state: State,
    bundle: Option<BundleDescriptor>,
    images: [Option<ImageDescriptor>; MAX_IMAGES],
    completed: usize,
    current: Option<ImageDescriptor>,
    received: u64,
    crc32: Crc32,
    claimed: bool,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Loader {
    pub const fn new() -> Self {
        Self {
            state: State::Idle,
            bundle: None,
            images: [None; MAX_IMAGES],
            completed: 0,
            current: None,
            received: 0,
            crc32: Crc32::new(),
            claimed: false,
        }
    }

    pub const fn state(&self) -> State {
        self.state
    }

    pub const fn received(&self) -> u64 {
        self.received
    }

    pub const fn claimed(&self) -> bool {
        self.claimed
    }

    pub fn disable(&mut self) {
        self.clear(State::Disabled);
    }

    fn clear(&mut self, state: State) {
        self.state = state;
        self.bundle = None;
        self.images.fill(None);
        self.completed = 0;
        self.current = None;
        self.received = 0;
        self.crc32 = Crc32::new();
    }

    fn reply(&self, request: Header, code: StatusCode) -> (Response, Action) {
        (Response::new(request, code, self.state, &[]), Action::None)
    }

    pub fn handle<S: Storage>(
        &mut self,
        message: &[u8],
        storage: &mut S,
    ) -> Result<(Response, Action), ProtocolError> {
        let header = Header::decode(message)?;
        if header.opcode & REPLY_FLAG != 0 {
            return Err(ProtocolError::Reply);
        }
        let body = &message[HEADER_SIZE..];
        let Some(opcode) = Opcode::decode(header.opcode) else {
            return Ok(self.reply(header, StatusCode::UnknownOpcode));
        };
        let empty = body.is_empty() && header.arg0 == 0 && header.arg1 == 0;
        match opcode {
            Opcode::Hello if empty => {
                let mut data = [0; 32];
                data[..8].copy_from_slice(&GUEST_RAM_BASE.to_le_bytes());
                data[8..16].copy_from_slice(&GUEST_RAM_END.to_le_bytes());
                data[16..20].copy_from_slice(&(MAX_BODY_SIZE as u32).to_le_bytes());
                data[20..24].copy_from_slice(&(BUNDLE_DESCRIPTOR_SIZE as u32).to_le_bytes());
                data[24..28].copy_from_slice(&(IMAGE_DESCRIPTOR_SIZE as u32).to_le_bytes());
                data[28..32].copy_from_slice(&(MAX_IMAGES as u32).to_le_bytes());
                Ok((
                    Response::new(header, StatusCode::Ok, self.state, &data),
                    Action::None,
                ))
            }
            Opcode::Status if empty => {
                let mut data = [0; 48];
                data[..4].copy_from_slice(&(self.state as u32).to_le_bytes());
                data[4..8].copy_from_slice(&u32::from(self.claimed).to_le_bytes());
                data[8..16].copy_from_slice(&self.received.to_le_bytes());
                if let Some(bundle) = self.bundle {
                    data[16..20].copy_from_slice(&bundle.image_count.to_le_bytes());
                }
                data[20..24].copy_from_slice(&(self.completed as u32).to_le_bytes());
                if let Some(image) = self.current {
                    data[24..32].copy_from_slice(&image.address.to_le_bytes());
                    data[32..40].copy_from_slice(&image.file_size.to_le_bytes());
                    data[40..48].copy_from_slice(&image.runtime_size.to_le_bytes());
                }
                Ok((
                    Response::new(header, StatusCode::Ok, self.state, &data),
                    Action::None,
                ))
            }
            Opcode::BeginBundle => {
                if self.state == State::Disabled {
                    return Ok(self.reply(header, StatusCode::GuestRunning));
                }
                if self.state != State::Idle {
                    return Ok(self.reply(header, StatusCode::BadState));
                }
                if header.arg0 != 0 || header.arg1 != 0 {
                    return Ok(self.reply(header, StatusCode::BadLength));
                }
                let Some(bundle) = BundleDescriptor::decode(body) else {
                    return Ok(self.reply(header, StatusCode::InvalidDescriptor));
                };
                self.bundle = Some(bundle);
                self.state = State::Bundle;
                self.claimed = true;
                Ok(self.reply(header, StatusCode::Ok))
            }
            Opcode::BeginImage => {
                if self.state != State::Bundle {
                    return Ok(self.reply(header, StatusCode::BadState));
                }
                if header.arg0 != 0 || header.arg1 != 0 {
                    return Ok(self.reply(header, StatusCode::BadLength));
                }
                let Some(bundle) = self.bundle else {
                    self.clear(State::Idle);
                    return Ok(self.reply(header, StatusCode::BadState));
                };
                if self.completed >= bundle.image_count as usize {
                    return Ok(self.reply(header, StatusCode::ImageCount));
                }
                let Some(image) = ImageDescriptor::decode(body) else {
                    return Ok(self.reply(header, StatusCode::InvalidDescriptor));
                };
                if self.images[..self.completed]
                    .iter()
                    .flatten()
                    .any(|other| image.overlaps(*other))
                {
                    return Ok(self.reply(header, StatusCode::Overlap));
                }
                self.current = Some(image);
                self.received = 0;
                self.crc32 = Crc32::new();
                self.state = State::Receiving;
                Ok(self.reply(header, StatusCode::Ok))
            }
            Opcode::Data => {
                if self.state != State::Receiving {
                    return Ok(self.reply(header, StatusCode::BadState));
                }
                let Some(image) = self.current else {
                    self.clear(State::Idle);
                    return Ok(self.reply(header, StatusCode::BadState));
                };
                if body.is_empty() {
                    return Ok(self.reply(header, StatusCode::BadLength));
                }
                if header.arg0 != self.received
                    || header.arg1 != 0
                    || self
                        .received
                        .checked_add(body.len() as u64)
                        .is_none_or(|end| end > image.file_size)
                {
                    return Ok(self.reply(header, StatusCode::BadOffset));
                }
                let Some(address) = image.address.checked_add(self.received) else {
                    return Ok(self.reply(header, StatusCode::BadOffset));
                };
                if storage.write(address, body).is_err() {
                    return Ok(self.reply(header, StatusCode::Storage));
                }
                self.crc32.update(body);
                self.received += body.len() as u64;
                let mut response = Response::new(header, StatusCode::Ok, self.state, &[]);
                response.header.arg1 = self.received;
                Ok((response, Action::None))
            }
            Opcode::EndImage if empty => {
                if self.state != State::Receiving {
                    return Ok(self.reply(header, StatusCode::BadState));
                }
                let Some(image) = self.current else {
                    self.clear(State::Idle);
                    return Ok(self.reply(header, StatusCode::BadState));
                };
                if self.received != image.file_size {
                    return Ok(self.reply(header, StatusCode::BadLength));
                }
                if self.crc32.finish() != image.crc32 {
                    return Ok(self.reply(header, StatusCode::Checksum));
                }
                let zero_size = image.runtime_size - image.file_size;
                if zero_size != 0
                    && storage
                        .zero(image.address + image.file_size, zero_size)
                        .is_err()
                {
                    return Ok(self.reply(header, StatusCode::Storage));
                }
                self.images[self.completed] = Some(image);
                self.completed += 1;
                self.current = None;
                self.received = 0;
                self.crc32 = Crc32::new();
                self.state = State::Bundle;
                Ok(self.reply(header, StatusCode::Ok))
            }
            Opcode::CommitBundle if empty => {
                if self.state != State::Bundle {
                    return Ok(self.reply(header, StatusCode::BadState));
                }
                let Some(bundle) = self.bundle else {
                    self.clear(State::Idle);
                    return Ok(self.reply(header, StatusCode::BadState));
                };
                if self.completed != bundle.image_count as usize {
                    return Ok(self.reply(header, StatusCode::ImageCount));
                }
                if !self.images[..self.completed]
                    .iter()
                    .flatten()
                    .any(|image| image.contains_entry(bundle.entry))
                {
                    return Ok(self.reply(header, StatusCode::Entry));
                }
                self.state = State::Ready;
                Ok(self.reply(header, StatusCode::Ok))
            }
            Opcode::Abort if empty => {
                if !matches!(self.state, State::Bundle | State::Receiving | State::Ready) {
                    return Ok(self.reply(header, StatusCode::BadState));
                }
                self.clear(State::Idle);
                Ok(self.reply(header, StatusCode::Ok))
            }
            Opcode::Boot if empty => {
                if self.state != State::Ready {
                    return Ok(self.reply(header, StatusCode::BadState));
                }
                let Some(bundle) = self.bundle else {
                    self.clear(State::Idle);
                    return Ok(self.reply(header, StatusCode::BadState));
                };
                self.clear(State::Disabled);
                Ok((
                    Response::new(header, StatusCode::Ok, self.state, &[]),
                    Action::Boot(bundle),
                ))
            }
            _ => Ok(self.reply(header, StatusCode::BadLength)),
        }
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, ProtocolError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(ProtocolError::Truncated)?;
    Ok(u16::from_le_bytes(
        value.try_into().map_err(|_| ProtocolError::Truncated)?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, ProtocolError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(ProtocolError::Truncated)?;
    Ok(u32::from_le_bytes(
        value.try_into().map_err(|_| ProtocolError::Truncated)?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, ProtocolError> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or(ProtocolError::Truncated)?;
    Ok(u64::from_le_bytes(
        value.try_into().map_err(|_| ProtocolError::Truncated)?,
    ))
}
