use switchvisor::{
    vdev::uart::{BASE, TxQueue, Uart},
    vdev::{self, DeviceError, VirtualDevice},
};

#[test]
fn scarlet_initialization_does_not_transmit_baud_divisors() {
    let mut uart = Uart::new();
    let device: &mut dyn VirtualDevice = &mut uart;
    for (register, value) in [(1, 0), (3, 0x80), (0, 3), (1, 0), (3, 3), (2, 7)] {
        device.write(register, 1, value).unwrap();
    }
    assert_eq!(device.read(5, 1), Ok(0x60));
    assert_eq!(device.read(2, 1), Ok(0xc1));
    assert!(uart.tx().is_empty());
    for &byte in b"Scarlet SMP\n\r\n\0\xff" {
        uart.write(0, 1, u64::from(byte)).unwrap();
    }
    let mut bytes = [0; 20];
    let length = uart.tx().peek(&mut bytes);
    assert_eq!(&bytes[..length], b"Scarlet SMP\n\r\n\0\xff");
    uart.write(2, 1, 7).unwrap();
    assert_eq!(uart.tx().len(), length); // Already accepted wire bytes survive FIFO clears.
}

#[test]
fn receive_fifo_reports_data_and_level_interrupt_state() {
    let mut uart = Uart::new();
    uart.write(1, 1, 15).unwrap();
    assert_eq!(uart.read(1, 1), Ok(0x05));
    assert_eq!(uart.read(0, 1), Ok(0));
    assert_eq!(uart.read(2, 1), Ok(1));
    assert_eq!(uart.read(5, 1), Ok(0x60));
    uart.write(2, 1, 1).unwrap();
    assert_eq!(uart.receive(b"abc"), 3);
    assert!(uart.interrupt_pending());
    assert_eq!(uart.read(2, 1), Ok(0xc4));
    assert_eq!(uart.read(5, 1), Ok(0x61));
    assert_eq!(uart.read(0, 1), Ok(u64::from(b'a')));
    assert_eq!(uart.read(0, 1), Ok(u64::from(b'b')));
    assert!(uart.interrupt_pending());
    assert_eq!(uart.read(0, 1), Ok(u64::from(b'c')));
    assert!(!uart.interrupt_pending());
    assert_eq!(uart.read(2, 1), Ok(0xc1));

    assert_eq!(uart.receive(b"discard"), 7);
    uart.write(2, 1, 3).unwrap();
    assert_eq!(uart.read(5, 1), Ok(0x60));
    assert!(!uart.interrupt_pending());

    uart.write(7, 1, 0xa5).unwrap();
    assert_eq!(uart.read(7, 1), Ok(0xa5));
    uart.write(4, 1, 0x1f).unwrap();
    assert_eq!(uart.read(6, 1), Ok(0xf0));
    uart.write(0, 1, 0x42).unwrap();
    assert_eq!(uart.read(0, 1), Ok(0));
}

#[test]
fn receive_overflow_sets_and_clears_line_status() {
    let mut uart = Uart::new();
    uart.write(1, 1, 4).unwrap();
    let bytes = [0x5a; switchvisor::vdev::uart::RX_CAPACITY + 1];
    assert_eq!(uart.receive(&bytes), bytes.len() - 1);
    assert!(uart.interrupt_pending());
    assert_eq!(uart.read(2, 1), Ok(0x06));
    assert_eq!(uart.read(5, 1), Ok(0x63));
    assert!(!uart.interrupt_pending());
    assert_eq!(uart.read(5, 1), Ok(0x61));
}

fn syndrome(register: u64, write: bool) -> u64 {
    (0x24 << 26) | (1 << 25) | (1 << 24) | (register << 16) | (u64::from(write) << 6) | 7
}

#[test]
fn common_dispatch_uses_ipa_and_handles_signed_and_zero_register_accesses() {
    let mut uart = Uart::new();
    uart.write(7, 1, 0x80).unwrap();
    let mut registers = [0xfeedface; 31];
    let hpfar = ((BASE + 7) >> 8) & !15;
    assert!(vdev::emulate(
        &mut uart,
        syndrome(4, false) | (1 << 21) | (1 << 15),
        0xf0000007,
        hpfar,
        &mut registers
    ));
    assert_eq!(registers[4], u64::MAX - 127);
    assert!(vdev::emulate(
        &mut uart,
        syndrome(5, false) | (1 << 21),
        BASE + 7,
        hpfar,
        &mut registers
    ));
    assert_eq!(registers[5], 0xffffff80);
    let before = registers;
    assert!(vdev::emulate(
        &mut uart,
        syndrome(31, false),
        BASE + 7,
        hpfar,
        &mut registers
    ));
    assert_eq!(registers, before);
    assert!(vdev::emulate(
        &mut uart,
        syndrome(31, true),
        BASE,
        BASE >> 8,
        &mut registers
    ));
    let mut bytes = [1];
    uart.tx().peek(&mut bytes);
    assert_eq!(bytes, [0]);
}

#[test]
fn invalid_width_or_register_does_not_change_device_or_registers() {
    let mut uart = Uart::new();
    assert_eq!(uart.write(0, 4, 0x42), Err(DeviceError::AccessSize));
    assert_eq!(uart.write(8, 1, 0x42), Err(DeviceError::Register));
    let mut registers = [0x55; 31];
    for (esr, address) in [
        (syndrome(0, true) ^ (1 << 24), BASE),
        (syndrome(0, true) | (2 << 22), BASE),
        (syndrome(0, false), BASE + 8),
    ] {
        assert!(!vdev::emulate(
            &mut uart,
            esr,
            address,
            (address >> 8) & !15,
            &mut registers
        ));
    }
    assert_eq!(registers, [0x55; 31]);
    assert!(uart.tx().is_empty());
}

#[test]
fn bounded_transport_queue_preserves_order_and_counts_disconnected_overflow() {
    let mut queue = TxQueue::<4>::new();
    for byte in 0..6 {
        queue.push(byte);
    }
    assert_eq!(queue.dropped(), 2);
    let mut bytes = [0; 4];
    queue.peek(&mut bytes);
    assert_eq!(bytes, [0, 1, 2, 3]);
    queue.consume(3);
    queue.push(6);
    queue.push(7);
    queue.push(8);
    queue.peek(&mut bytes);
    assert_eq!(bytes, [3, 6, 7, 8]);
    queue.consume(99);
    assert!(queue.is_empty());
    let mut zero = TxQueue::<0>::new();
    zero.push(1);
    zero.consume(1);
    assert_eq!(zero.dropped(), 1);
    assert_eq!(zero.peek(&mut bytes), 0);
}
