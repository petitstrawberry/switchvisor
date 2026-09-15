//! Binary protocol and state machine for pre-guest raw payload uploads.

use crate::payload::{Crc32, LOAD_BASE, MAX_RUNTIME_SIZE, RESIDENT_BASE, RESIDENT_SIZE};

pub const MAGIC: [u8; 4] = *b"SWVL";
pub const VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 32;
pub const MAX_MESSAGE_SIZE: usize = 4096;
pub const MAX_BODY_SIZE: usize = MAX_MESSAGE_SIZE - HEADER_SIZE;
pub const DESCRIPTOR_SIZE: usize = 96;
pub const MAX_RESPONSE_SIZE: usize = HEADER_SIZE + 48;
pub const REPLY_FLAG: u16 = 0x8000;
pub const PRESERVE_BOOT_ARGS: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Opcode {
    Hello = 1,
    Begin = 2,
    Data = 3,
    Commit = 4,
    Abort = 5,
    Boot = 6,
    Status = 7,
}

impl Opcode {
    fn decode(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Begin,
            3 => Self::Data,
            4 => Self::Commit,
            5 => Self::Abort,
            6 => Self::Boot,
            7 => Self::Status,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum State {
    Idle = 0,
    Receiving = 1,
    Ready = 2,
    Disabled = 3,
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
        let opcode = u16_at(bytes, 6)?;
        let header = Self {
            opcode,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub file_size: u64,
    pub runtime_size: u64,
    pub entry_offset: u64,
    pub flags: u32,
    pub crc32: u32,
    pub registers: [u64; 8],
}

impl Descriptor {
    pub fn preserve_boot_args(self) -> bool {
        self.flags & PRESERVE_BOOT_ARGS != 0
    }

    pub fn entry(self) -> u64 {
        LOAD_BASE + self.entry_offset
    }

    pub fn validate(self) -> bool {
        let Some(end) = LOAD_BASE.checked_add(self.runtime_size) else {
            return false;
        };
        let resident_end = RESIDENT_BASE + RESIDENT_SIZE;
        self.file_size != 0
            && self.runtime_size >= self.file_size
            && self.runtime_size <= MAX_RUNTIME_SIZE
            && self.entry_offset % 4 == 0
            && self
                .entry_offset
                .checked_add(4)
                .is_some_and(|entry_end| entry_end <= self.file_size)
            && self.flags & !PRESERVE_BOOT_ARGS == 0
            && (!self.preserve_boot_args() || self.registers.iter().all(|value| *value == 0))
            && !(LOAD_BASE < resident_end && RESIDENT_BASE < end)
    }

    pub fn encode(self, output: &mut [u8; DESCRIPTOR_SIZE]) {
        output[..8].copy_from_slice(&self.file_size.to_le_bytes());
        output[8..16].copy_from_slice(&self.runtime_size.to_le_bytes());
        output[16..24].copy_from_slice(&self.entry_offset.to_le_bytes());
        output[24..28].copy_from_slice(&self.flags.to_le_bytes());
        output[28..32].copy_from_slice(&self.crc32.to_le_bytes());
        for (index, value) in self.registers.iter().enumerate() {
            output[32 + index * 8..40 + index * 8].copy_from_slice(&value.to_le_bytes());
        }
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != DESCRIPTOR_SIZE {
            return None;
        }
        let mut registers = [0; 8];
        for (index, value) in registers.iter_mut().enumerate() {
            *value = u64_at(bytes, 32 + index * 8).ok()?;
        }
        let descriptor = Self {
            file_size: u64_at(bytes, 0).ok()?,
            runtime_size: u64_at(bytes, 8).ok()?,
            entry_offset: u64_at(bytes, 16).ok()?,
            flags: u32_at(bytes, 24).ok()?,
            crc32: u32_at(bytes, 28).ok()?,
            registers,
        };
        descriptor.validate().then_some(descriptor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageError;

pub trait Storage {
    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), StorageError>;
    fn zero(&mut self, offset: u64, length: u64) -> Result<(), StorageError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Boot(Descriptor),
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
    descriptor: Option<Descriptor>,
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
            descriptor: None,
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
        self.state = State::Disabled;
        self.descriptor = None;
        self.received = 0;
        self.crc32 = Crc32::new();
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
            return Ok((
                Response::new(header, StatusCode::UnknownOpcode, self.state, &[]),
                Action::None,
            ));
        };
        let empty = body.is_empty() && header.arg0 == 0 && header.arg1 == 0;
        match opcode {
            Opcode::Hello if empty => {
                let mut data = [0; 24];
                data[..8].copy_from_slice(&MAX_RUNTIME_SIZE.to_le_bytes());
                data[8..16].copy_from_slice(&LOAD_BASE.to_le_bytes());
                data[16..20].copy_from_slice(&(MAX_BODY_SIZE as u32).to_le_bytes());
                data[20..24].copy_from_slice(&(DESCRIPTOR_SIZE as u32).to_le_bytes());
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
                if let Some(descriptor) = self.descriptor {
                    data[16..24].copy_from_slice(&descriptor.file_size.to_le_bytes());
                    data[24..32].copy_from_slice(&descriptor.runtime_size.to_le_bytes());
                    data[32..40].copy_from_slice(&descriptor.entry_offset.to_le_bytes());
                    data[40..44].copy_from_slice(&descriptor.crc32.to_le_bytes());
                    data[44..48].copy_from_slice(&descriptor.flags.to_le_bytes());
                }
                Ok((
                    Response::new(header, StatusCode::Ok, self.state, &data),
                    Action::None,
                ))
            }
            Opcode::Begin => {
                if self.state == State::Disabled {
                    return Ok((
                        Response::new(header, StatusCode::GuestRunning, self.state, &[]),
                        Action::None,
                    ));
                }
                if self.state != State::Idle {
                    return Ok((
                        Response::new(header, StatusCode::BadState, self.state, &[]),
                        Action::None,
                    ));
                }
                if header.arg0 != 0 || header.arg1 != 0 {
                    return Ok((
                        Response::new(header, StatusCode::BadLength, self.state, &[]),
                        Action::None,
                    ));
                }
                let Some(descriptor) = Descriptor::decode(body) else {
                    return Ok((
                        Response::new(header, StatusCode::InvalidDescriptor, self.state, &[]),
                        Action::None,
                    ));
                };
                self.state = State::Receiving;
                self.descriptor = Some(descriptor);
                self.received = 0;
                self.crc32 = Crc32::new();
                self.claimed = true;
                Ok((
                    Response::new(header, StatusCode::Ok, self.state, &[]),
                    Action::None,
                ))
            }
            Opcode::Data => {
                if self.state != State::Receiving {
                    return Ok((
                        Response::new(header, StatusCode::BadState, self.state, &[]),
                        Action::None,
                    ));
                }
                let Some(descriptor) = self.descriptor else {
                    self.state = State::Idle;
                    return Ok((
                        Response::new(header, StatusCode::BadState, self.state, &[]),
                        Action::None,
                    ));
                };
                if body.is_empty() {
                    return Ok((
                        Response::new(header, StatusCode::BadLength, self.state, &[]),
                        Action::None,
                    ));
                }
                if header.arg0 != self.received
                    || header.arg1 != 0
                    || self
                        .received
                        .checked_add(body.len() as u64)
                        .is_none_or(|end| end > descriptor.file_size)
                {
                    return Ok((
                        Response::new(header, StatusCode::BadOffset, self.state, &[]),
                        Action::None,
                    ));
                }
                if storage.write(self.received, body).is_err() {
                    return Ok((
                        Response::new(header, StatusCode::Storage, self.state, &[]),
                        Action::None,
                    ));
                }
                self.crc32.update(body);
                self.received += body.len() as u64;
                let mut response = Response::new(header, StatusCode::Ok, self.state, &[]);
                response.header.arg1 = self.received;
                Ok((response, Action::None))
            }
            Opcode::Commit if empty => {
                if self.state != State::Receiving {
                    return Ok((
                        Response::new(header, StatusCode::BadState, self.state, &[]),
                        Action::None,
                    ));
                }
                let Some(descriptor) = self.descriptor else {
                    self.state = State::Idle;
                    return Ok((
                        Response::new(header, StatusCode::BadState, self.state, &[]),
                        Action::None,
                    ));
                };
                if self.received != descriptor.file_size {
                    return Ok((
                        Response::new(header, StatusCode::BadLength, self.state, &[]),
                        Action::None,
                    ));
                }
                if self.crc32.finish() != descriptor.crc32 {
                    return Ok((
                        Response::new(header, StatusCode::Checksum, self.state, &[]),
                        Action::None,
                    ));
                }
                if storage
                    .zero(
                        descriptor.file_size,
                        descriptor.runtime_size - descriptor.file_size,
                    )
                    .is_err()
                {
                    return Ok((
                        Response::new(header, StatusCode::Storage, self.state, &[]),
                        Action::None,
                    ));
                }
                self.state = State::Ready;
                Ok((
                    Response::new(header, StatusCode::Ok, self.state, &[]),
                    Action::None,
                ))
            }
            Opcode::Abort if empty => {
                if !matches!(self.state, State::Receiving | State::Ready) {
                    return Ok((
                        Response::new(header, StatusCode::BadState, self.state, &[]),
                        Action::None,
                    ));
                }
                self.state = State::Idle;
                self.descriptor = None;
                self.received = 0;
                self.crc32 = Crc32::new();
                Ok((
                    Response::new(header, StatusCode::Ok, self.state, &[]),
                    Action::None,
                ))
            }
            Opcode::Boot if empty => {
                if self.state != State::Ready {
                    return Ok((
                        Response::new(header, StatusCode::BadState, self.state, &[]),
                        Action::None,
                    ));
                }
                let Some(descriptor) = self.descriptor else {
                    self.state = State::Idle;
                    return Ok((
                        Response::new(header, StatusCode::BadState, self.state, &[]),
                        Action::None,
                    ));
                };
                self.state = State::Disabled;
                self.descriptor = None;
                Ok((
                    Response::new(header, StatusCode::Ok, self.state, &[]),
                    Action::Boot(descriptor),
                ))
            }
            _ => Ok((
                Response::new(header, StatusCode::BadLength, self.state, &[]),
                Action::None,
            )),
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
