use core::fmt;

use crate::{BL33_LOAD_BASE, IPA_LIMIT, SCARLET_LOAD_BASE, fdt::Fdt, memory::AddressRange};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    Truncated,
    Header,
    RuntimeExtent,
    Fdt(crate::fdt::FdtError),
    HekatePatchLayout,
    InvalidUartPort,
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "image {self:?}")
    }
}

impl From<crate::fdt::FdtError> for ImageError {
    fn from(error: crate::fdt::FdtError) -> Self {
        Self::Fdt(error)
    }
}

fn le64(bytes: &[u8], offset: usize) -> Result<u64, ImageError> {
    let data = bytes.get(offset..offset + 8).ok_or(ImageError::Truncated)?;
    Ok(u64::from_le_bytes(
        data.try_into().map_err(|_| ImageError::Truncated)?,
    ))
}

#[derive(Debug)]
pub struct UbootImage {
    pub linked_base: u64,
    pub end_offset: u64,
    pub bss_start_offset: u64,
    pub bss_end_offset: u64,
    pub control_fdt_size: usize,
    pub runtime: AddressRange,
}

impl UbootImage {
    pub fn parse(bytes: &[u8]) -> Result<Self, ImageError> {
        let first = bytes.get(..4).ok_or(ImageError::Truncated)?;
        if u32::from_le_bytes(first.try_into().map_err(|_| ImageError::Truncated)?) & 0xfc00_0000
            != 0x1400_0000
        {
            return Err(ImageError::Header);
        }
        let linked_base = le64(bytes, 8)?;
        let end_offset = le64(bytes, 16)?;
        let bss_start_offset = le64(bytes, 24)?;
        let bss_end_offset = le64(bytes, 32)?;
        if end_offset < 40
            || end_offset > u32::MAX as u64
            || end_offset % 8 != 0
            || bss_start_offset < end_offset
            || bss_end_offset < bss_start_offset
        {
            return Err(ImageError::Header);
        }
        let offset = usize::try_from(end_offset).map_err(|_| ImageError::Truncated)?;
        let fdt = Fdt::parse(bytes.get(offset..).ok_or(ImageError::Truncated)?)?;
        let size = bss_end_offset.max(bytes.len() as u64);
        let runtime =
            AddressRange::new(BL33_LOAD_BASE, size).map_err(|_| ImageError::RuntimeExtent)?;
        if runtime.end() > IPA_LIMIT || linked_base.checked_add(size).is_none() {
            return Err(ImageError::RuntimeExtent);
        }
        Ok(Self {
            linked_base,
            end_offset,
            bss_start_offset,
            bss_end_offset,
            control_fdt_size: fdt.total_size(),
            runtime,
        })
    }

    /// Validate fixed-offset strcpy targets used by the pinned Hekate loader.
    pub fn validate_hekate_patches(&self, bytes: &[u8]) -> Result<(), ImageError> {
        let offset = usize::try_from(self.end_offset).map_err(|_| ImageError::Truncated)?;
        let fdt = Fdt::parse(bytes.get(offset..).ok_or(ImageError::Truncated)?)?;
        for (path, expected) in [
            ("/serial@70006000", 0x1c94),
            ("/serial@70006040", 0x1dc0),
            ("/serial@70006200", 0x1eec),
        ] {
            let property = fdt
                .find_property(path, "status")?
                .ok_or(ImageError::HekatePatchLayout)?;
            if property.data_offset != expected || property.data.len() < 5 {
                return Err(ImageError::HekatePatchLayout);
            }
        }
        for (name, expected) in [("stdout-path", 0x3f34), ("stderr-path", 0x3f54)] {
            let property = fdt
                .find_property("/chosen", name)?
                .ok_or(ImageError::HekatePatchLayout)?;
            if property.data_offset + 8 != expected
                || property.data.len() < 17
                || !property.data.starts_with(b"/serial@")
            {
                return Err(ImageError::HekatePatchLayout);
            }
        }
        Ok(())
    }

    /// Host-side reproduction of Hekate's writes; validate every target before mutation.
    pub fn apply_hekate_uart_patch(&self, bytes: &mut [u8], port: u8) -> Result<(), ImageError> {
        if port > 3 {
            return Err(ImageError::InvalidUartPort);
        }
        self.validate_hekate_patches(bytes)?;
        if port == 0 {
            return Ok(());
        }
        let offset = usize::try_from(self.end_offset).map_err(|_| ImageError::Truncated)?;
        let address = [b"70006000\0", b"70006040\0", b"70006200\0"][usize::from(port - 1)];
        for target in [0x3f34, 0x3f54] {
            bytes[offset + target..offset + target + address.len()].copy_from_slice(address);
        }
        let target = offset + 0x1c94 + usize::from(port - 1) * 0x12c;
        bytes[target..target + 5].copy_from_slice(b"okay\0");
        Ok(())
    }
}

#[derive(Debug)]
pub struct ScarletImage {
    pub text_offset: u64,
    pub image_size: u64,
    pub runtime: AddressRange,
}

impl ScarletImage {
    /// Inspect a raw, uncompressed Linux Image. U-Boot remains responsible for uImage/gzip.
    pub fn parse(bytes: &[u8]) -> Result<Self, ImageError> {
        if bytes.len() < 64 {
            return Err(ImageError::Truncated);
        }
        if &bytes[56..60] != b"ARM\x64" {
            return Err(ImageError::Header);
        }
        let text_offset = le64(bytes, 8)?;
        let image_size = le64(bytes, 16)?;
        let flags = le64(bytes, 24)?;
        if text_offset != 0x20_0000 || flags & 1 != 0 {
            return Err(ImageError::Header);
        }
        if image_size < bytes.len() as u64 {
            return Err(ImageError::RuntimeExtent);
        }
        let runtime = AddressRange::new(SCARLET_LOAD_BASE, image_size)
            .map_err(|_| ImageError::RuntimeExtent)?;
        if runtime.end() > 0x8d00_0000 {
            return Err(ImageError::RuntimeExtent);
        }
        Ok(Self {
            text_offset,
            image_size,
            runtime,
        })
    }
}
