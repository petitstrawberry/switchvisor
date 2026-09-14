//! Allocation-free boot contracts shared by the EL2 implementation and host tools.
#![no_std]
#![forbid(unsafe_code)]

pub mod fdt;
pub mod framebuffer;
pub mod image;
pub mod memory;
pub mod payload;

pub const CPU_COUNT: usize = 4;
pub const IPA_LIMIT: u64 = 1 << 36;
pub const PAGE_SIZE: u64 = 4096;
pub const BLOCK_SIZE: u64 = 2 * 1024 * 1024;
pub const HV_SIZE_BUDGET: u64 = 16 * 1024 * 1024;
pub const BL33_LOAD_BASE: u64 = 0xaa00_0000;
pub const SCARLET_LOAD_BASE: u64 = 0x8020_0000;
