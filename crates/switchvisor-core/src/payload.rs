//! External raw EL1 payloads. Placement follows the fixed initial BL33 layout.
use core::fmt;

use crate::BL33_LOAD_BASE;

pub const LOAD_BASE: u64 = BL33_LOAD_BASE;
pub const RESIDENT_BASE: u64 = 0xfec0_0000;
pub const RESIDENT_SIZE: u64 = crate::HV_SIZE_BUDGET;
pub const MAX_PACKAGE_SIZE: u64 = 64 * 1024 * 1024;
pub const MAX_RUNTIME_SIZE: u64 = 64 * 1024 * 1024;
pub const STACK_TOP: u64 = 0x8a80_0000;
pub const CONFIG_OFFSET: usize = 4096;
pub const CONFIG_SIZE: usize = 256;
pub const EXIT_HVC: u16 = 0x5356;

const MAGIC: &[u8; 8] = b"SVRAW001";
const CONFIG_CRC_OFFSET: usize = 132;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadError {
    Truncated,
    Header,
    Checksum,
    PackageRange,
    RuntimeRange,
    Entry,
}

impl fmt::Display for PayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "payload {self:?}")
    }
}

pub fn crc32(bytes: &[u8]) -> u32 {
    checksum(bytes.iter().copied())
}

fn checksum(bytes: impl Iterator<Item = u8>) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn config_crc(bytes: &[u8]) -> u32 {
    checksum(bytes.iter().enumerate().map(|(i, byte)| {
        if (CONFIG_CRC_OFFSET..CONFIG_CRC_OFFSET + 4).contains(&i) {
            0
        } else {
            *byte
        }
    }))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, PayloadError> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or(PayloadError::Truncated)?;
    Ok(u32::from_le_bytes(
        data.try_into().map_err(|_| PayloadError::Truncated)?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, PayloadError> {
    let data = bytes
        .get(offset..offset + 8)
        .ok_or(PayloadError::Truncated)?;
    Ok(u64::from_le_bytes(
        data.try_into().map_err(|_| PayloadError::Truncated)?,
    ))
}

/// The payload is opaque to the VMM: no U-Boot or OS header is required.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Payload {
    pub package_size: u64,
    pub bootstrap_size: u64,
    pub offset: u64,
    pub file_size: u64,
    pub runtime_size: u64,
    pub entry_offset: u64,
    pub registers: [u64; 8],
    pub preserve_boot_args: bool,
    pub crc32: u32,
}

impl Payload {
    pub fn entry(self) -> Result<u64, PayloadError> {
        self.validate()?;
        LOAD_BASE
            .checked_add(self.entry_offset)
            .ok_or(PayloadError::Entry)
    }

    pub fn validate(self) -> Result<(), PayloadError> {
        if self.bootstrap_size < (CONFIG_OFFSET + CONFIG_SIZE) as u64
            || self.bootstrap_size % 16 != 0
            || self.offset < self.bootstrap_size
            || self.offset % 16 != 0
            || self.offset.checked_add(self.file_size) != Some(self.package_size)
            || self.package_size > MAX_PACKAGE_SIZE
        {
            return Err(PayloadError::PackageRange);
        }
        if self.file_size == 0
            || self.runtime_size < self.file_size
            || self.runtime_size > MAX_RUNTIME_SIZE
        {
            return Err(PayloadError::RuntimeRange);
        }
        if self.entry_offset % 4 != 0
            || self
                .entry_offset
                .checked_add(4)
                .is_none_or(|end| end > self.file_size)
        {
            return Err(PayloadError::Entry);
        }
        if self.preserve_boot_args && self.registers.iter().any(|value| *value != 0) {
            return Err(PayloadError::Header);
        }
        Ok(())
    }

    pub fn source(self, package: &[u8]) -> Result<&[u8], PayloadError> {
        self.validate()?;
        if package.len() as u64 != self.package_size {
            return Err(PayloadError::PackageRange);
        }
        let source = package
            .get(self.offset as usize..)
            .ok_or(PayloadError::PackageRange)?;
        if crc32(source) != self.crc32 {
            return Err(PayloadError::Checksum);
        }
        Ok(source)
    }

    pub fn encode(self) -> Result<[u8; CONFIG_SIZE], PayloadError> {
        self.validate()?;
        let mut bytes = [0u8; CONFIG_SIZE];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&u32::from(self.preserve_boot_args).to_le_bytes());
        for (i, value) in [
            self.package_size,
            self.bootstrap_size,
            self.offset,
            self.file_size,
            self.runtime_size,
            self.entry_offset,
        ]
        .iter()
        .enumerate()
        {
            bytes[16 + i * 8..24 + i * 8].copy_from_slice(&value.to_le_bytes());
        }
        for (i, value) in self.registers.iter().enumerate() {
            bytes[64 + i * 8..72 + i * 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes[128..132].copy_from_slice(&self.crc32.to_le_bytes());
        let crc = config_crc(&bytes);
        bytes[CONFIG_CRC_OFFSET..CONFIG_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        Ok(bytes)
    }

    /// A zero descriptor selects the diagnostic-only entry path.
    pub fn decode(bytes: &[u8]) -> Result<Option<Self>, PayloadError> {
        if bytes.len() != CONFIG_SIZE {
            return Err(PayloadError::Truncated);
        }
        if bytes.iter().all(|b| *b == 0) {
            return Ok(None);
        }
        if bytes.get(..8) != Some(MAGIC.as_slice())
            || u32_at(bytes, 8)? != 1
            || u32_at(bytes, 12)? > 1
        {
            return Err(PayloadError::Header);
        }
        if config_crc(bytes) != u32_at(bytes, CONFIG_CRC_OFFSET)? {
            return Err(PayloadError::Checksum);
        }
        let mut registers = [0u64; 8];
        for (i, value) in registers.iter_mut().enumerate() {
            *value = u64_at(bytes, 64 + i * 8)?;
        }
        let payload = Self {
            package_size: u64_at(bytes, 16)?,
            bootstrap_size: u64_at(bytes, 24)?,
            offset: u64_at(bytes, 32)?,
            file_size: u64_at(bytes, 40)?,
            runtime_size: u64_at(bytes, 48)?,
            entry_offset: u64_at(bytes, 56)?,
            registers,
            preserve_boot_args: u32_at(bytes, 12)? != 0,
            crc32: u32_at(bytes, 128)?,
        };
        payload.validate()?;
        if payload.encode()?.as_slice() != bytes {
            return Err(PayloadError::Header);
        }
        Ok(Some(payload))
    }
}
