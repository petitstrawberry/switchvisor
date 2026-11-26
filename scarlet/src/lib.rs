//! Private control interface between the Switchvisor device and swvctl.
//! Reads take a cursor in the first word and replace the buffer with the HVC
//! reply. Start reads an IPA and length in the first two words. Stop takes no
//! buffer. The handle-control return value carries the status for each call.
#![no_std]

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod driver;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use driver::force_link;

pub const DEVICE_NAME: &str = "switchvisor";
pub const DEVICE_PATH: &str = "/dev/switchvisor";
pub type Words = [u64; 8];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Operation {
    ReadLog = 0x5357_0001,
    StartTrace = 0x5357_0002,
    StopTrace = 0x5357_0003,
    ReadTrace = 0x5357_0004,
}

impl Operation {
    pub fn from_command(command: u32) -> Option<Self> {
        match command {
            n if n == Self::ReadLog as u32 => Some(Self::ReadLog),
            n if n == Self::StartTrace as u32 => Some(Self::StartTrace),
            n if n == Self::StopTrace as u32 => Some(Self::StopTrace),
            n if n == Self::ReadTrace as u32 => Some(Self::ReadTrace),
            _ => None,
        }
    }

    pub fn function(self) -> u64 {
        match self {
            Self::ReadLog => 0x0001_0001,
            Self::StartTrace => 0x0002_0001,
            Self::StopTrace => 0x0002_0002,
            Self::ReadTrace => 0x0002_0003,
        }
    }

    pub fn reads(self) -> bool {
        matches!(self, Self::ReadLog | Self::ReadTrace)
    }
}

pub const OK: i32 = 0;
pub const PERMISSION_DENIED: i32 = 1;
pub const BAD_BUFFER: i32 = 2;
pub const UNSUPPORTED: i32 = 3;
pub const INVALID_ARGUMENT: i32 = 4;
pub const BUSY: i32 = 5;
