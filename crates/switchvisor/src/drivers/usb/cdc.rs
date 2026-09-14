//! USB 2.0 CDC ACM control protocol. Host output is discarded; guest RX is absent.

pub const VID: u16 = 0x1209;
pub const PID: u16 = 0x0001; // pid.codes test PID, for development builds.
pub const CONTROL_SIZE: usize = 256;

#[derive(Clone, Copy, Debug)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    pub fn from_words(words: [u32; 2]) -> Self {
        Self {
            request_type: words[0] as u8,
            request: (words[0] >> 8) as u8,
            value: (words[0] >> 16) as u16,
            index: words[1] as u16,
            length: (words[1] >> 16) as u16,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Data(usize),
    LineCoding,
    Status,
    Address(u8),
    Configuration(u8),
    Halt { endpoint: u8, halted: bool },
    Stall,
}

pub struct Acm {
    pub line_coding: [u8; 7],
    pub dtr: bool,
    pub configuration: u8,
    halted: u8,
}

impl Default for Acm {
    fn default() -> Self {
        Self::new()
    }
}

impl Acm {
    pub const fn new() -> Self {
        Self {
            line_coding: [0x00, 0xc2, 0x01, 0x00, 0, 0, 8],
            dtr: false,
            configuration: 0,
            halted: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn setup(&mut self, s: Setup, output: &mut [u8; CONTROL_SIZE], high_speed: bool) -> Reply {
        let size = match (s.request_type, s.request) {
            (0x80, 6) if s.index == 0 || s.value >> 8 == 3 => match (s.value >> 8, s.value as u8) {
                (1, 0) => copy(
                    output,
                    &[
                        18,
                        1,
                        0,
                        2,
                        0xef,
                        2,
                        1,
                        64,
                        VID as u8,
                        (VID >> 8) as u8,
                        PID as u8,
                        (PID >> 8) as u8,
                        0,
                        1,
                        1,
                        2,
                        3,
                        1,
                    ],
                ),
                (2 | 7, 0) => {
                    let high = if s.value >> 8 == 7 {
                        !high_speed
                    } else {
                        high_speed
                    };
                    let packet: u16 = if high { 512 } else { 64 };
                    copy(
                        output,
                        &[
                            9,
                            (s.value >> 8) as u8,
                            75,
                            0,
                            2,
                            1,
                            0,
                            0xc0,
                            1,
                            8,
                            11,
                            0,
                            2,
                            2,
                            2,
                            1,
                            0,
                            9,
                            4,
                            0,
                            0,
                            1,
                            2,
                            2,
                            1,
                            0,
                            5,
                            0x24,
                            0,
                            0x10,
                            0x01,
                            5,
                            0x24,
                            1,
                            0,
                            1,
                            4,
                            0x24,
                            2,
                            2,
                            5,
                            0x24,
                            6,
                            0,
                            1,
                            7,
                            5,
                            0x82,
                            3,
                            16,
                            0,
                            if high { 9 } else { 16 },
                            9,
                            4,
                            1,
                            0,
                            2,
                            0x0a,
                            0,
                            0,
                            0,
                            7,
                            5,
                            0x01,
                            2,
                            packet as u8,
                            (packet >> 8) as u8,
                            0,
                            7,
                            5,
                            0x81,
                            2,
                            packet as u8,
                            (packet >> 8) as u8,
                            0,
                        ],
                    );
                    75
                }
                (3, 0) => copy(output, &[4, 3, 9, 4]),
                (3, index @ 1..=3) if s.index == 0x0409 || s.index == 0 => {
                    let text = match index {
                        1 => "Switchvisor",
                        2 => "Switchvisor USB console",
                        _ => "SWV0001",
                    };
                    let size = 2 + text.len() * 2;
                    output[0] = size as u8;
                    output[1] = 3;
                    for (index, byte) in text.bytes().enumerate() {
                        output[2 + index * 2] = byte;
                        output[3 + index * 2] = 0;
                    }
                    size
                }
                (6, 0) => copy(output, &[10, 6, 0, 2, 0xef, 2, 1, 64, 1, 0]),
                _ => return Reply::Stall,
            },
            (0x00, 5) if s.index == 0 && s.length == 0 && s.value < 128 => {
                return Reply::Address(s.value as u8);
            }
            (0x00, 9) if s.index == 0 && s.length == 0 && s.value <= 1 => {
                return Reply::Configuration(s.value as u8);
            }
            (0x80, 8) if s.value == 0 && s.index == 0 && s.length == 1 => {
                copy(output, &[self.configuration])
            }
            (0x80, 0) if s.value == 0 && s.index == 0 && s.length == 2 => copy(output, &[1, 0]),
            (0x81, 0) if s.value == 0 && s.index <= 1 && s.length == 2 => copy(output, &[0, 0]),
            (0x81, 10) if s.value == 0 && s.index <= 1 && s.length == 1 => copy(output, &[0]),
            (0x01, 11) if s.value == 0 && s.index <= 1 && s.length == 0 => return Reply::Status,
            (0x82, 0) if s.value == 0 && s.length == 2 => {
                let Some(endpoint) = endpoint(s.index) else {
                    return Reply::Stall;
                };
                copy(output, &[u8::from(self.halted & (1 << endpoint) != 0), 0])
            }
            (0x02, request @ (1 | 3)) if s.value == 0 && s.length == 0 => {
                let Some(endpoint @ (2 | 3 | 5)) = endpoint(s.index) else {
                    return Reply::Stall;
                };
                let halted = request == 3;
                if halted {
                    self.halted |= 1 << endpoint;
                } else {
                    self.halted &= !(1 << endpoint);
                }
                return Reply::Halt { endpoint, halted };
            }
            (0x21, 0x20)
                if s.index == 0 && s.value == 0 && s.length == 7 && self.configuration == 1 =>
            {
                return Reply::LineCoding;
            }
            (0xa1, 0x21)
                if s.index == 0 && s.value == 0 && s.length == 7 && self.configuration == 1 =>
            {
                copy(output, &self.line_coding)
            }
            (0x21, 0x22)
                if s.index == 0 && s.value <= 3 && s.length == 0 && self.configuration == 1 =>
            {
                self.dtr = s.value & 1 != 0;
                return Reply::Status;
            }
            (0x21, 0x23) if s.index == 0 && s.length == 0 && self.configuration == 1 => {
                return Reply::Status;
            }
            _ => return Reply::Stall,
        };
        Reply::Data(size.min(usize::from(s.length)))
    }
}

fn copy(output: &mut [u8; CONTROL_SIZE], data: &[u8]) -> usize {
    output[..data.len()].copy_from_slice(data);
    data.len()
}

fn endpoint(address: u16) -> Option<u8> {
    match address {
        0 | 0x80 => Some(0),
        0x01 => Some(2),
        0x81 => Some(3),
        0x82 => Some(5),
        _ => None,
    }
}
