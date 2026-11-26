//! Opt-in EL1 driver for the Switchvisor control node in the boot DT.
extern crate alloc;

use crate::{
    BAD_BUFFER, BUSY, DEVICE_NAME, INVALID_ARGUMENT, OK, Operation, PERMISSION_DENIED, UNSUPPORTED,
    Words,
};
use alloc::{string::String, sync::Arc, vec};
use core::{any::Any, arch::asm};
use scarlet::{
    arch::Trapframe,
    device::{
        Device, DeviceType,
        manager::{DeviceManager, DriverPriority},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    library::std::usercopy::{copy_from_user, copy_to_user},
    object::capability::{
        ControlOps, MemoryMappingInfo, MemoryMappingOps,
        selectable::{ReadyInterest, SelectWaitOutcome, Selectable},
    },
    shield::access::InodeAccess,
    task::mytask,
};

struct Switchvisor {
    operator: u32,
}

impl Device for Switchvisor {
    fn device_type(&self) -> DeviceType {
        DeviceType::Char
    }

    fn name(&self) -> &'static str {
        DEVICE_NAME
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl ControlOps for Switchvisor {
    fn control(&self, command: u32, address: usize) -> Result<i32, &'static str> {
        let Some(task) = mytask() else {
            return Ok(PERMISSION_DENIED);
        };
        // Use the current credential identity for every request, including
        // requests made with a handle received from another process.
        if InodeAccess::for_creation(0).owner.principal.0 != self.operator {
            return Ok(PERMISSION_DENIED);
        }
        let Some(operation) = Operation::from_command(command) else {
            return Ok(UNSUPPORTED);
        };

        let mut input = [0; 16];
        let length = match operation {
            Operation::ReadLog | Operation::ReadTrace => 8,
            Operation::StartTrace => 16,
            Operation::StopTrace => 0,
        };
        if length != 0 && copy_from_user(&task, address, &mut input[..length]).is_err() {
            return Ok(BAD_BUFFER);
        }
        let arg0 = u64::from_le_bytes(input[..8].try_into().unwrap());
        let arg1 = u64::from_le_bytes(input[8..].try_into().unwrap());
        let reply = hypercall(operation, arg0, arg1);
        match reply[0] as i64 {
            -2 => return Ok(INVALID_ARGUMENT),
            -3 => return Ok(BUSY),
            code if code < 0 => return Ok(UNSUPPORTED),
            _ => (),
        }

        if operation.reads() {
            let mut output = [0; core::mem::size_of::<Words>()];
            for (word, dst) in reply.into_iter().zip(output.chunks_exact_mut(8)) {
                dst.copy_from_slice(&word.to_le_bytes());
            }
            if copy_to_user(&task, address, &output).is_err() {
                return Ok(BAD_BUFFER);
            }
        }
        Ok(OK)
    }
}

impl MemoryMappingOps for Switchvisor {
    fn get_mapping_info(&self, _: usize, _: usize) -> Result<MemoryMappingInfo, &'static str> {
        Err("Switchvisor control has no memory mapping")
    }

    fn supports_mmap(&self) -> bool {
        false
    }
}

impl Selectable for Switchvisor {
    fn wait_until_ready(
        &self,
        _: ReadyInterest,
        _: &mut Trapframe,
        _: Option<u64>,
        _: u64,
    ) -> SelectWaitOutcome {
        SelectWaitOutcome::Ready
    }
}

fn hypercall(operation: Operation, arg0: u64, arg1: u64) -> Words {
    let (mut x0, mut x1, mut x2) = (operation.function(), arg0, arg1);
    let (x3, x4, x5, x6, x7);
    // SAFETY: The device binds only to the resident boot's control node.
    // Every operand is passed by value; user addresses never reach EL2.
    unsafe {
        asm!(
            "hvc #0x5357",
            inout("x0") x0, inout("x1") x1, inout("x2") x2,
            inout("x3") 0u64 => x3, inout("x4") 0u64 => x4,
            inout("x5") 0u64 => x5, inout("x6") 0u64 => x6,
            inout("x7") 0u64 => x7,
            options(nostack),
        );
    }
    [x0, x1, x2, x3, x4, x5, x6, x7]
}

fn probe(info: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let operator = info
        .property("switchvisor,operator")
        .and_then(|property| property.value().try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or("missing Switchvisor operator principal")?;
    DeviceManager::get_manager().register_device_with_name(
        String::from(DEVICE_NAME),
        Arc::new(Switchvisor { operator }),
    );
    Ok(())
}

fn register() {
    use alloc::boxed::Box;
    let driver = PlatformDeviceDriver::new(
        "switchvisor-control",
        probe,
        |_| Ok(()),
        vec!["switchvisor,control-v1"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register);

pub fn force_link() {
    core::hint::black_box(register as fn());
}
