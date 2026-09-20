//! Guest-visible virtual devices. Physical device drivers live under `drivers`.
pub mod gicv2;
pub mod lic;
pub mod uart;
pub mod usb_ownership;
pub mod virtio_net;

use crate::mmio::Access;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmioRegion {
    pub base: u64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceError {
    AccessSize,
    Register,
}

/// Object-safe MMIO contract, independent of physical device drivers.
/// Rejected operations must not change device state.
pub trait VirtualDevice {
    fn region(&self) -> MmioRegion;
    fn read(&mut self, offset: u64, size: u8) -> Result<u64, DeviceError>;
    fn write(&mut self, offset: u64, size: u8, value: u64) -> Result<(), DeviceError>;
}

/// Decode before dispatch. The caller advances ELR only when this returns true.
pub fn emulate(
    device: &mut dyn VirtualDevice,
    esr: u64,
    far: u64,
    hpfar: u64,
    registers: &mut [u64; 31],
) -> bool {
    let region = device.region();
    let Some(access) = Access::decode_region(esr, far, hpfar, region.base, region.size) else {
        return false;
    };
    emulate_access(device, access, registers)
}

/// Dispatch an access that was decoded from the faulting instruction because
/// the hardware did not provide a valid instruction syndrome in ESR_EL2.
pub fn emulate_access(
    device: &mut dyn VirtualDevice,
    access: Access,
    registers: &mut [u64; 31],
) -> bool {
    if access.write {
        device
            .write(access.offset, access.size, access.store_data(registers))
            .is_ok()
    } else {
        match device.read(access.offset, access.size) {
            Ok(value) => {
                access.load_data(value, registers);
                true
            }
            Err(_) => false,
        }
    }
}
