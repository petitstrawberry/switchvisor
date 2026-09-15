//! Allocation-free command parsing for the EL2 control CDC port.

pub const MAX_LINE_SIZE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Ping,
    Status,
    Reboot,
    RebootRcm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    InvalidEncoding,
    LineTooLong,
    UnknownCommand,
}

pub struct Parser {
    bytes: [u8; MAX_LINE_SIZE],
    length: usize,
    error: Option<ParseError>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub const fn new() -> Self {
        Self {
            bytes: [0; MAX_LINE_SIZE],
            length: 0,
            error: None,
        }
    }

    /// Consume one byte. A result is returned only when a complete non-empty
    /// line or a malformed line ends at `\n`.
    pub fn push(&mut self, byte: u8) -> Option<Result<Command, ParseError>> {
        if byte == b'\n' {
            let error = self.error.take();
            let mut length = self.length;
            self.length = 0;
            if length != 0 && self.bytes[length - 1] == b'\r' {
                length -= 1;
            }
            if let Some(error) = error {
                return Some(Err(error));
            }
            if length == 0 {
                return None;
            }
            return Some(match &self.bytes[..length] {
                b"ping" => Ok(Command::Ping),
                b"status" => Ok(Command::Status),
                b"reboot" => Ok(Command::Reboot),
                b"reboot-rcm" => Ok(Command::RebootRcm),
                _ => Err(ParseError::UnknownCommand),
            });
        }
        if self.error.is_some() {
            return None;
        }
        if !byte.is_ascii() || (byte.is_ascii_control() && byte != b'\r') {
            self.error = Some(ParseError::InvalidEncoding);
        } else if self.length == self.bytes.len() {
            self.error = Some(ParseError::LineTooLong);
        } else {
            self.bytes[self.length] = byte;
            self.length += 1;
        }
        None
    }
}
