#[path = "support/network.rs"]
mod support;
use support::*;
use switchvisor::{
    net::{FRAME_SIZE, GUEST_MAC},
    vdev::{VirtualDevice, virtio_net::Net},
};

#[test]
fn negotiates_modern_features_and_requires_version_one() {
    let mut net = Net::new();
    assert_eq!(net.read(0, 4), Ok(0x74726976));
    assert_eq!(net.read(4, 4), Ok(2));
    assert_eq!(net.read(8, 4), Ok(1));
    for (i, b) in GUEST_MAC.into_iter().enumerate() {
        assert_eq!(net.read(0x100 + i as u64, 1), Ok(u64::from(b)));
    }
    net.write(0x70, 4, 11).unwrap();
    assert_eq!(net.read(0x70, 4).unwrap() & 8, 0);
    configure(&mut net);
    net.write(0x30, 4, u32::MAX.into()).unwrap();
    assert_eq!(net.read(0x34, 4), Ok(0));
    assert!(net.write(0x70, 1, 0).is_err());
    assert!(net.running());
    net.write(0x70, 4, 0).unwrap();
    assert!(!net.running());
    assert!(!net.interrupt_pending());
}

#[test]
fn transfers_scattered_packets_and_publishes_used_entries_and_interrupts() {
    let mut memory = Memory::new();
    let mut net = Net::new();
    configure(&mut net);
    let frame = frame();
    memory.tx(&frame);
    // Header split across descriptors, not assumed to be contiguous with data.
    memory.descriptor(1, 0, RAM + 0x7000, 5, 1, 1);
    memory.descriptor(1, 1, RAM + 0x7005, 7 + frame.len(), 0, 0);
    let mut out = [0; FRAME_SIZE];
    assert_eq!(net.transmit(&mut memory, &mut out), frame.len());
    assert_eq!(&out[..frame.len()], frame);
    assert_eq!(memory.get16(0x5002), 1);
    assert_eq!(&memory.bytes[0x5008..0x500c], &[0; 4]);
    assert!(net.interrupt_pending());
    net.write(0x64, 4, 1).unwrap();
    assert!(!net.interrupt_pending());
    memory.rx();
    memory.descriptor(0, 0, RAM + 0x8000, 12, 3, 1);
    memory.descriptor(0, 1, RAM + 0x9000, FRAME_SIZE, 2, 0);
    assert!(net.receive(&mut memory, &frame));
    assert_eq!(
        &memory.bytes[0x8000..0x800c],
        &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]
    );
    assert_eq!(&memory.bytes[0x9000..0x9000 + frame.len()], frame);
    assert_eq!(memory.get16(0x2002), 1);
    assert_eq!(memory.bytes[0x2008], (12 + frame.len()) as u8);
}

#[test]
fn invalid_descriptors_fail_closed_without_guest_writes_or_hangs() {
    for case in 0..8 {
        let mut memory = Memory::new();
        let mut net = Net::new();
        configure(&mut net);
        memory.tx(&frame());
        match case {
            0 => memory.descriptor(1, 0, 0xfec00000, 72, 0, 0),
            1 => memory.descriptor(1, 0, u64::MAX - 4, 72, 0, 0),
            2 => memory.descriptor(1, 0, RAM + 0x7000, 72, 1, 0),
            3 => memory.descriptor(1, 0, RAM + 0x7000, 72, 4, 0),
            4 => memory.descriptor(1, 0, RAM + 0x7000, 72, 2, 0),
            5 => memory.put16(0x4002, 9),
            6 => memory.put16(0x4004, 8),
            7 => memory.bytes[0x7001] = 1,
            _ => unreachable!(),
        }
        let mut out = [0; FRAME_SIZE];
        assert_eq!(net.transmit(&mut memory, &mut out), 0, "{case}");
        assert_ne!(net.read(0x70, 4).unwrap() & 64, 0);
        assert!(!net.running());
        assert_eq!(memory.writes, 0);
        net.write(0x70, 4, 0).unwrap();
        assert_eq!(net.read(0x60, 4), Ok(0));
    }
}

#[test]
fn invalid_queue_addresses_and_short_rx_buffers_are_never_written() {
    for invalid_address in [true, false] {
        let mut memory = Memory::new();
        let mut net = Net::new();
        configure(&mut net);
        memory.rx();
        if invalid_address {
            net.write(0x30, 4, 0).unwrap();
            net.write(0x44, 4, 0).unwrap();
            net.write(0xa0, 4, 0xfec00000).unwrap();
            net.write(0x44, 4, 1).unwrap();
        } else {
            memory.descriptor(0, 0, RAM + 0x8000, 12, 2, 0);
        }
        assert!(!net.receive(&mut memory, &frame()));
        assert_eq!(memory.writes, 0);
        assert!(!net.running());
    }
}

#[test]
fn ring_indices_wrap_and_notification_suppression_is_respected() {
    let mut memory = Memory::new();
    let mut net = Net::new();
    configure(&mut net);
    memory.put16(0x4000, 1);
    let frame = frame();
    let mut out = [0; FRAME_SIZE];
    for _ in 0..65537 {
        memory.tx(&frame);
        assert_eq!(net.transmit(&mut memory, &mut out), 60);
    }
    assert_eq!(memory.get16(0x5002), 1);
    assert!(!net.interrupt_pending());
    net.set_link(true);
    assert_eq!(net.read(0x60, 4), Ok(2));
    assert_eq!(net.read(0x106, 2), Ok(1));
    net.write(0x64, 4, 2).unwrap();
    assert!(!net.interrupt_pending());
}

#[test]
fn empty_tx_waits_for_its_queue_notification_without_reading_guest_ram() {
    let mut memory = Memory::new();
    let mut net = Net::new();
    configure(&mut net);
    let mut out = [0; FRAME_SIZE];
    assert_eq!(net.transmit(&mut memory, &mut out), 0);
    let reads = memory.reads;
    assert_eq!(reads, 1);

    // A posted descriptor becomes visible to the device when the driver kicks
    // TX. RX notifications and invalid queue indices must not wake TX.
    let frame = frame();
    memory.tx(&frame);
    for queue in [0, 2, u64::from(u32::MAX)] {
        net.write(0x50, 4, queue).unwrap();
        for _ in 0..100 {
            assert_eq!(net.transmit(&mut memory, &mut out), 0);
        }
    }
    assert_eq!(memory.reads, reads);
    assert_eq!(memory.writes, 0);

    net.write(0x50, 4, 1).unwrap();
    assert_eq!(net.transmit(&mut memory, &mut out), frame.len());
    assert_eq!(&out[..frame.len()], frame);
    assert_eq!(memory.get16(0x5002), 1);
    assert_eq!(net.transmit(&mut memory, &mut out), 0);
    let reads = memory.reads;
    assert_eq!(net.transmit(&mut memory, &mut out), 0);
    assert_eq!(memory.reads, reads);

    // Reset and DRIVER_OK must discover buffers posted during setup, even if
    // the old queue had already gone idle.
    net.write(0x70, 4, 0).unwrap();
    memory = Memory::new();
    memory.tx(&frame);
    configure(&mut net);
    assert_eq!(net.transmit(&mut memory, &mut out), frame.len());
}

#[test]
fn stalled_rx_resumes_when_the_driver_replenishes_and_notifies_it() {
    let mut memory = Memory::new();
    let mut net = Net::new();
    configure(&mut net);
    let frame = frame();
    assert!(!net.receive(&mut memory, &frame));
    let reads = memory.reads;
    assert_eq!(reads, 1);
    memory.rx();
    net.write(0x50, 4, 1).unwrap();
    for _ in 0..100 {
        assert!(!net.receive(&mut memory, &frame));
    }
    assert_eq!(memory.reads, reads);
    assert_eq!(memory.writes, 0);
    net.write(0x50, 4, 0).unwrap();
    assert!(net.receive(&mut memory, &frame));
    assert_eq!(&memory.bytes[0x800c..0x800c + frame.len()], frame);
    assert_eq!(memory.get16(0x2002), 1);
}

#[test]
fn direct_payload_copies_cross_every_header_boundary_without_writing_slack() {
    let frame: Vec<u8> = (0..FRAME_SIZE).map(|n| (n * 37) as u8).collect();
    for split in [1, 5, 11, 12, 13, 14, 63, 64, 1513, 1525] {
        let mut memory = Memory::new();
        let mut net = Net::new();
        configure(&mut net);
        let mut packet = vec![0; 12];
        packet.extend_from_slice(&frame);
        memory.bytes[0x7000..0x7000 + split].copy_from_slice(&packet[..split]);
        memory.bytes[0x9000..0x9000 + packet.len() - split].copy_from_slice(&packet[split..]);
        memory.descriptor(1, 0, RAM + 0x7000, split, 1, 1);
        memory.descriptor(1, 1, RAM + 0x9000, packet.len() - split, 0, 0);
        memory.post(1, 0);
        let mut output = [0; FRAME_SIZE];
        assert_eq!(net.transmit(&mut memory, &mut output), frame.len());
        assert_eq!(output.as_slice(), frame);

        memory.bytes[0xa000..0xb800].fill(0xa5);
        memory.descriptor(0, 0, RAM + 0xa000, split, 3, 1);
        memory.descriptor(0, 1, RAM + 0xb000, packet.len() - split + 7, 2, 0);
        memory.post(0, 0);
        assert!(net.receive(&mut memory, &frame));
        packet[10] = 1;
        assert_eq!(&memory.bytes[0xa000..0xa000 + split], &packet[..split]);
        assert_eq!(
            &memory.bytes[0xb000..0xb000 + packet.len() - split],
            &packet[split..]
        );
        assert!(
            memory.bytes[0xb000 + packet.len() - split..0xb007 + packet.len() - split]
                .iter()
                .all(|&b| b == 0xa5)
        );

        // The descriptor snapshot is reusable; a shorter next chain must not
        // revisit its old tail.
        memory.tx(&frame);
        assert_eq!(net.transmit(&mut memory, &mut output), frame.len());
        assert_eq!(output.as_slice(), frame);
    }
}
