//! Reusable CDC ACM function state.

pub const VID: u16 = 0x1209;
pub const PID: u16 = 0x0001; // pid.codes test PID, for development builds.

#[derive(Clone, Copy, Debug)]
pub struct Acm {
    pub line_coding: [u8; 7],
    pub dtr: bool,
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
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}
