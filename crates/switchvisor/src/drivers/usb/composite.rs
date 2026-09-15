//! USB descriptors and EP0 routing for the Switchvisor composite device.

use super::cdc::{Acm, PID, VID};

pub const CONTROL_SIZE: usize = 256;
pub const ACM_COUNT: usize = 2;
pub const CONFIGURATION_SIZE: usize = 164;

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
    LineCoding(usize),
    Status,
    Address(u8),
    Configuration(u8),
    Halt { endpoint: u8, halted: bool },
    Stall,
}

pub struct Composite {
    pub acm: [Acm; ACM_COUNT],
    pub configuration: u8,
    halted: u16,
}

impl Default for Composite {
    fn default() -> Self {
        Self::new()
    }
}

impl Composite {
    pub const fn new() -> Self {
        Self {
            acm: [Acm::new(); ACM_COUNT],
            configuration: 0,
            halted: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn setup(
        &mut self,
        setup: Setup,
        output: &mut [u8; CONTROL_SIZE],
        high_speed: bool,
    ) -> Reply {
        let size = match (setup.request_type, setup.request) {
            (0x80, 6) => match (setup.value >> 8, setup.value as u8) {
                (1, 0) if setup.index == 0 => copy(
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
                        1,
                        1,
                        1,
                        2,
                        3,
                        1,
                    ],
                ),
                (2 | 7, 0) if setup.index == 0 => {
                    let high = if setup.value >> 8 == 7 {
                        !high_speed
                    } else {
                        high_speed
                    };
                    configuration(output, (setup.value >> 8) as u8, high)
                }
                (3, 0) if setup.index == 0 => copy(output, &[4, 3, 9, 4]),
                (3, index @ 1..=6) if setup.index == 0x0409 || setup.index == 0 => {
                    string(output, index)
                }
                (6, 0) if setup.index == 0 => copy(output, &[10, 6, 0, 2, 0xef, 2, 1, 64, 1, 0]),
                _ => return Reply::Stall,
            },
            (0x00, 5) if setup.index == 0 && setup.length == 0 && setup.value < 128 => {
                return Reply::Address(setup.value as u8);
            }
            (0x00, 9) if setup.index == 0 && setup.length == 0 && setup.value <= 1 => {
                return Reply::Configuration(setup.value as u8);
            }
            (0x80, 8) if setup.value == 0 && setup.index == 0 && setup.length == 1 => {
                copy(output, &[self.configuration])
            }
            (0x80, 0) if setup.value == 0 && setup.index == 0 && setup.length == 2 => {
                copy(output, &[1, 0])
            }
            (0x81, 0) if setup.value == 0 && setup.index <= 4 && setup.length == 2 => {
                copy(output, &[0, 0])
            }
            (0x81, 10) if setup.value == 0 && setup.index <= 4 && setup.length == 1 => {
                copy(output, &[0])
            }
            (0x01, 11) if setup.value == 0 && setup.index <= 4 && setup.length == 0 => {
                return Reply::Status;
            }
            (0x82, 0) if setup.value == 0 && setup.length == 2 => {
                let Some(endpoint) = endpoint(setup.index) else {
                    return Reply::Stall;
                };
                copy(output, &[u8::from(self.halted & (1 << endpoint) != 0), 0])
            }
            (0x02, request @ (1 | 3))
                if setup.value == 0 && setup.length == 0 && self.configuration == 1 =>
            {
                let Some(endpoint @ (2 | 3 | 5 | 6 | 7 | 9 | 10 | 11)) = endpoint(setup.index)
                else {
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
            (request_type @ (0x21 | 0xa1), request) if self.configuration == 1 => {
                let Some(function) = acm_function(setup.index) else {
                    return Reply::Stall;
                };
                match (request_type, request) {
                    (0x21, 0x20) if setup.value == 0 && setup.length == 7 => {
                        return Reply::LineCoding(function);
                    }
                    (0xa1, 0x21) if setup.value == 0 && setup.length == 7 => {
                        copy(output, &self.acm[function].line_coding)
                    }
                    (0x21, 0x22) if setup.value <= 3 && setup.length == 0 => {
                        self.acm[function].dtr = setup.value & 1 != 0;
                        return Reply::Status;
                    }
                    (0x21, 0x23) if setup.length == 0 => return Reply::Status,
                    _ => return Reply::Stall,
                }
            }
            _ => return Reply::Stall,
        };
        Reply::Data(size.min(usize::from(setup.length)))
    }
}

fn configuration(output: &mut [u8; CONTROL_SIZE], descriptor_type: u8, high: bool) -> usize {
    let packet: u16 = if high { 512 } else { 64 };
    let interval = if high { 9 } else { 16 };
    let mut cursor = 0;
    append(
        output,
        &mut cursor,
        &[
            9,
            descriptor_type,
            CONFIGURATION_SIZE as u8,
            (CONFIGURATION_SIZE >> 8) as u8,
            5,
            1,
            0,
            0xc0,
            1,
        ],
    );
    append_cdc(
        output,
        &mut cursor,
        0,
        0x82,
        0x01,
        0x81,
        4,
        packet,
        interval,
    );
    append_cdc(
        output,
        &mut cursor,
        2,
        0x84,
        0x03,
        0x83,
        5,
        packet,
        interval,
    );
    append(
        output,
        &mut cursor,
        &[
            9,
            4,
            4,
            0,
            2,
            0xff,
            0x53,
            1,
            6,
            7,
            5,
            0x05,
            2,
            packet as u8,
            (packet >> 8) as u8,
            0,
            7,
            5,
            0x85,
            2,
            packet as u8,
            (packet >> 8) as u8,
            0,
        ],
    );
    debug_assert_eq!(cursor, CONFIGURATION_SIZE);
    cursor
}

#[allow(clippy::too_many_arguments)]
fn append_cdc(
    output: &mut [u8; CONTROL_SIZE],
    cursor: &mut usize,
    control_interface: u8,
    notification_endpoint: u8,
    out_endpoint: u8,
    in_endpoint: u8,
    string_index: u8,
    packet: u16,
    interval: u8,
) {
    let data_interface = control_interface + 1;
    append(
        output,
        cursor,
        &[
            8,
            11,
            control_interface,
            2,
            2,
            2,
            1,
            string_index,
            9,
            4,
            control_interface,
            0,
            1,
            2,
            2,
            1,
            string_index,
            5,
            0x24,
            0,
            0x10,
            0x01,
            5,
            0x24,
            1,
            0,
            data_interface,
            4,
            0x24,
            2,
            2,
            5,
            0x24,
            6,
            control_interface,
            data_interface,
            7,
            5,
            notification_endpoint,
            3,
            16,
            0,
            interval,
            9,
            4,
            data_interface,
            0,
            2,
            0x0a,
            0,
            0,
            string_index,
            7,
            5,
            out_endpoint,
            2,
            packet as u8,
            (packet >> 8) as u8,
            0,
            7,
            5,
            in_endpoint,
            2,
            packet as u8,
            (packet >> 8) as u8,
            0,
        ],
    );
}

fn string(output: &mut [u8; CONTROL_SIZE], index: u8) -> usize {
    let text = match index {
        1 => "Switchvisor",
        2 => "Switchvisor USB",
        3 => "SWV0001",
        4 => "Guest console",
        5 => "Switchvisor control",
        6 => "Payload loader",
        _ => return 0,
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

fn append(output: &mut [u8; CONTROL_SIZE], cursor: &mut usize, data: &[u8]) {
    output[*cursor..*cursor + data.len()].copy_from_slice(data);
    *cursor += data.len();
}

fn copy(output: &mut [u8; CONTROL_SIZE], data: &[u8]) -> usize {
    output[..data.len()].copy_from_slice(data);
    data.len()
}

fn acm_function(interface: u16) -> Option<usize> {
    match interface {
        0 => Some(0),
        2 => Some(1),
        _ => None,
    }
}

fn endpoint(address: u16) -> Option<u8> {
    match address {
        0 | 0x80 => Some(0),
        0x01 => Some(2),
        0x81 => Some(3),
        0x82 => Some(5),
        0x03 => Some(6),
        0x83 => Some(7),
        0x84 => Some(9),
        0x05 => Some(10),
        0x85 => Some(11),
        _ => None,
    }
}
