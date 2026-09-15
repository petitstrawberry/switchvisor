//! Allocation-free device implementations and policies shared by the hypervisor and host tools.
#![no_std]
#![forbid(unsafe_code)]

pub mod drivers;
pub mod el2_mmu;
pub mod fdt;
pub mod fdt_memory;
pub mod framebuffer;
pub mod image;
pub mod mc;
pub mod memory;
pub mod mmio;
pub mod payload;
pub mod psci;
pub mod stage2;
pub mod vdev;

pub const CPU_COUNT: usize = 4;
pub const IPA_LIMIT: u64 = 1 << 36;
pub const PAGE_SIZE: u64 = 4096;
pub const BLOCK_SIZE: u64 = 2 * 1024 * 1024;
pub const HV_SIZE_BUDGET: u64 = 16 * 1024 * 1024;
pub const BL33_LOAD_BASE: u64 = 0xaa00_0000;
