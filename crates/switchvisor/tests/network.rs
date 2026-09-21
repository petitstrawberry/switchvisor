#[path = "support/network.rs"]
mod support;
use support::*;
use switchvisor::net::*;
use switchvisor::vdev::VirtualDevice;

fn ip_packet(protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0; 34 + payload.len()];
    frame[..6].copy_from_slice(&MANAGEMENT_MAC);
    frame[6..12].copy_from_slice(&HOST_MAC);
    frame[12..14].copy_from_slice(&[8, 0]);
    frame[14] = 0x45;
    frame[16..18].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    frame[22] = 64;
    frame[23] = protocol;
    frame[26..30].copy_from_slice(&[192, 168, 77, 2]);
    frame[30..34].copy_from_slice(&MANAGEMENT_IP);
    frame[34..].copy_from_slice(payload);
    let sum = checksum(&frame[14..34]);
    frame[24..26].copy_from_slice(&sum.to_be_bytes());
    frame
}

#[test]
fn bridge_preserves_guest_ethernet_and_terminates_management_arp() {
    let mut bridge = Bridge::new();
    let request = arp(HOST_MAC, [192, 168, 77, 2]);
    assert!(bridge.receive(Port::Host, &request));
    assert_eq!(bridge.guest.front(), Some(request.as_slice()));
    let reply = bridge.host.front().unwrap();
    assert_eq!(&reply[..6], &HOST_MAC);
    assert_eq!(&reply[6..12], &MANAGEMENT_MAC);
    assert_eq!(&reply[28..32], &MANAGEMENT_IP);
    assert_eq!(&reply[38..42], &[192, 168, 77, 2]);
    assert_eq!(reply[21], 2);
    bridge.host.pop();
    bridge.guest.pop();
    let bytes = frame();
    bridge.receive(Port::Guest, &bytes);
    assert_eq!(bridge.host.front(), Some(bytes.as_slice()));
    assert!(bridge.guest.front().is_none());
}

#[test]
fn management_ping_and_read_only_udp_work_while_guest_is_stalled() {
    let mut bridge = Bridge::new();
    let mut echo = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1, 0x55];
    let sum = checksum(&echo);
    echo[2..4].copy_from_slice(&sum.to_be_bytes());
    let packet = ip_packet(1, &echo);
    while !bridge.guest.full() {
        bridge.receive(Port::Host, &frame());
    }
    assert!(bridge.receive(Port::Host, &packet));
    let reply = bridge.host.front().unwrap();
    assert_eq!(reply[34], 0);
    assert_eq!(&reply[38..43], &echo[4..]);
    assert_eq!(checksum(&reply[14..34]), 0);
    assert_eq!(checksum(&reply[34..43]), 0);
    bridge.host.pop();
    for command in [b"ping".as_slice(), b"status\n", b"reboot", b"reboot-rcm"] {
        let mut udp = vec![0; 8 + command.len()];
        udp[..2].copy_from_slice(&50000u16.to_be_bytes());
        udp[2..4].copy_from_slice(&MANAGEMENT_PORT.to_be_bytes());
        let length = udp.len() as u16;
        udp[4..6].copy_from_slice(&length.to_be_bytes());
        udp[8..].copy_from_slice(command);
        bridge.receive(Port::Host, &ip_packet(17, &udp));
        if command.starts_with(b"reboot") {
            assert!(bridge.host.front().is_none());
        } else {
            let reply = bridge.host.front().unwrap();
            assert_eq!(&reply[36..38], &50000u16.to_be_bytes());
            assert!(reply[42..].starts_with(if command == b"ping" {
                b"pong\n"
            } else {
                b"switchvisor"
            }));
            bridge.host.pop();
        }
    }
}

#[test]
fn malformed_ip_checksums_lengths_fragments_and_arp_are_ignored() {
    let mut echo = [8, 0, 0, 0, 0, 0, 0, 0];
    let sum = checksum(&echo);
    echo[2..4].copy_from_slice(&sum.to_be_bytes());
    let valid = ip_packet(1, &echo);
    for end in 0..valid.len() {
        let mut bridge = Bridge::new();
        bridge.receive(Port::Host, &valid[..end]);
        assert!(bridge.host.front().is_none());
    }
    for index in [14, 16, 20, 22, 24, 30, 34, 36] {
        let mut packet = valid.clone();
        packet[index] ^= 1;
        let mut bridge = Bridge::new();
        bridge.receive(Port::Host, &packet);
        assert!(bridge.host.front().is_none(), "{index}");
    }
    let mut bridge = Bridge::new();
    let mut packet = arp(HOST_MAC, [192, 168, 77, 2]);
    packet[22] ^= 2;
    bridge.receive(Port::Host, &packet);
    assert!(bridge.host.front().is_none());
}

struct Link {
    up: bool,
    input: Vec<Vec<u8>>,
    output: Vec<Vec<u8>>,
    busy: bool,
}
impl Ethernet for Link {
    fn link_up(&self) -> bool {
        self.up
    }
    fn receive_frame(&mut self, receive: impl FnOnce(&[u8])) -> bool {
        if self.input.is_empty() {
            return false;
        }
        let frame = self.input.remove(0);
        receive(&frame);
        true
    }
    fn send_frame(&mut self, frame: &[u8]) -> bool {
        if !self.up || self.busy {
            return false;
        }
        self.output.push(frame.to_vec());
        true
    }
}

#[test]
fn bridge_and_virtqueues_forward_both_directions_and_backpressure_tx() {
    let mut network = Network::new();
    configure(&mut network.device);
    let mut memory = Memory::new();
    memory.rx();
    let frame = frame();
    memory.tx(&frame);
    let mut link = Link {
        up: true,
        input: vec![frame.clone()],
        output: vec![],
        busy: true,
    };
    network.service(&mut memory, &mut link);
    assert_eq!(&memory.bytes[0x800c..0x800c + frame.len()], frame);
    assert!(link.output.is_empty());
    assert!(network.bridge.host.front().is_some());
    link.busy = false;
    network.service(&mut memory, &mut link);
    assert_eq!(link.output, vec![frame]);
    network.bridge.host.push(&support::frame());
    link.up = false;
    network.service(&mut memory, &mut link);
    assert!(network.bridge.host.front().is_none());
}

#[test]
fn full_guest_rx_queue_does_not_block_guest_tx() {
    let mut network = Network::new();
    configure(&mut network.device);
    let mut memory = Memory::new();
    let mut link = Link {
        up: true,
        input: vec![],
        output: vec![],
        busy: true,
    };
    network.service(&mut memory, &mut link);

    // No guest RX descriptors are available. A driver can still need to send
    // a TCP ACK before it reposts RX buffers, so TX must make independent progress.
    let frame = frame();
    while !network.bridge.guest.full() {
        assert!(network.bridge.guest.push(&frame));
    }
    let mut queued_for_host = 0;
    while !network.bridge.host.full() {
        assert!(network.bridge.host.push(&frame));
        queued_for_host += 1;
    }
    memory.tx(&frame);

    network.device.write(0x50, 4, 1).unwrap();
    network.service(&mut memory, &mut link);
    assert_eq!(memory.get16(0x5002), 0); // Host egress still backpressures TX.
    assert!(link.output.is_empty());

    link.busy = false;
    network.service(&mut memory, &mut link);
    assert_eq!(memory.get16(0x5002), 1); // TX completed without any RX progress.
    assert_eq!(link.output, vec![frame; queued_for_host + 1]);
    assert!(network.bridge.guest.full());
    assert!(network.bridge.host.front().is_none());
}

struct CopyProbe {
    memory: Memory,
    rx_source: usize,
    tx_target: usize,
}
impl switchvisor::vdev::virtio_net::GuestMemory for CopyProbe {
    fn valid(&self, address: u64, length: usize) -> bool {
        self.memory.valid(address, length)
    }
    fn read(
        &mut self,
        address: u64,
        output: &mut [u8],
    ) -> Result<(), switchvisor::vdev::virtio_net::MemoryError> {
        if address == RAM + 0x700c {
            self.tx_target = output.as_ptr() as usize;
        }
        self.memory.read(address, output)
    }
    fn write(
        &mut self,
        address: u64,
        bytes: &[u8],
    ) -> Result<(), switchvisor::vdev::virtio_net::MemoryError> {
        if address == RAM + 0x800c {
            self.rx_source = bytes.as_ptr() as usize;
        }
        self.memory.write(address, bytes)
    }
    fn barrier(&mut self) {
        self.memory.barrier();
    }
}

#[test]
fn network_passes_receive_storage_directly_and_fills_transmit_ring_in_place() {
    let mut network = Network::new();
    configure(&mut network.device);
    let mut memory = CopyProbe {
        memory: Memory::new(),
        rx_source: 0,
        tx_target: 0,
    };
    memory.memory.rx();
    memory.memory.tx(&frame());
    let mut link = Link {
        up: true,
        input: vec![frame()],
        output: vec![],
        busy: true,
    };
    let input_pointer = link.input[0].as_ptr() as usize;
    network.service(&mut memory, &mut link);
    assert_eq!(memory.rx_source, input_pointer); // No packet staging or bridge copy.
    assert_eq!(
        memory.tx_target,
        network.bridge.host.front().unwrap().as_ptr() as usize
    );
    assert_eq!(&memory.memory.bytes[0x800c..0x800c + 60], frame());
}

#[test]
fn direct_rx_preserves_order_after_backpressure_and_recycled_slots_zero_padding() {
    let mut network = Network::new();
    configure(&mut network.device);
    let mut memory = Memory::new();
    let first = frame();
    let mut second = first.clone();
    second[20] ^= 1;
    let mut link = Link {
        up: true,
        input: vec![first.clone()],
        output: vec![],
        busy: false,
    };
    network.service(&mut memory, &mut link);
    assert_eq!(network.bridge.guest.front(), Some(first.as_slice()));
    memory.rx();
    network.device.write(0x50, 4, 0).unwrap();
    link.input.push(second.clone());
    network.service(&mut memory, &mut link);
    assert_eq!(&memory.bytes[0x800c..0x800c + first.len()], first);
    assert_eq!(network.bridge.guest.front(), Some(second.as_slice()));

    let mut bridge = Bridge::new();
    for _ in 0..8 {
        bridge.host.push(&[0xa5; FRAME_SIZE]);
        bridge.host.pop();
    }
    bridge.receive(Port::Host, &arp(HOST_MAC, [192, 168, 77, 2]));
    assert!(bridge.host.front().unwrap()[42..60].iter().all(|&b| b == 0));
    bridge.host.pop();
    let mut udp = vec![0; 12];
    udp[..2].copy_from_slice(&50000u16.to_be_bytes());
    udp[2..4].copy_from_slice(&MANAGEMENT_PORT.to_be_bytes());
    udp[4..6].copy_from_slice(&12u16.to_be_bytes());
    udp[8..].copy_from_slice(b"ping");
    bridge.receive(Port::Host, &ip_packet(17, &udp));
    let reply = bridge.host.front().unwrap();
    assert_eq!(&reply[40..42], &[0, 0]);
    assert!(reply[47..60].iter().all(|&b| b == 0));
}

#[test]
fn management_udp_checksums_cover_even_and_odd_payloads_without_packet_staging() {
    for command in [b"ping".as_slice(), b"ping\n"] {
        let mut udp = vec![0; 8 + command.len()];
        udp[..2].copy_from_slice(&50000u16.to_be_bytes());
        udp[2..4].copy_from_slice(&MANAGEMENT_PORT.to_be_bytes());
        let length = udp.len() as u16;
        udp[4..6].copy_from_slice(&length.to_be_bytes());
        udp[8..].copy_from_slice(command);
        let mut pseudo = vec![192, 168, 77, 2, 192, 168, 77, 1, 0, 17];
        pseudo.extend_from_slice(&length.to_be_bytes());
        pseudo.extend_from_slice(&udp);
        let sum = checksum(&pseudo);
        udp[6..8].copy_from_slice(&(if sum == 0 { 0xffff } else { sum }).to_be_bytes());
        let mut bridge = Bridge::new();
        bridge.receive(Port::Host, &ip_packet(17, &udp));
        assert_eq!(&bridge.host.front().unwrap()[42..47], b"pong\n");
        bridge.host.pop();
        udp[6] ^= 1;
        bridge.receive(Port::Host, &ip_packet(17, &udp));
        assert!(bridge.host.empty());
    }
}

#[test]
fn idle_and_backpressured_service_does_not_repeatedly_scan_guest_ram() {
    let mut network = Network::new();
    configure(&mut network.device);
    let mut memory = Memory::new();
    let mut link = Link {
        up: false,
        input: vec![],
        output: vec![],
        busy: true,
    };
    network.service(&mut memory, &mut link);
    assert_eq!(memory.reads, 1); // Check TX avail.idx once, then return to EL1.
    assert_eq!(memory.writes, 0);
    network.bridge.guest.push(&frame());
    memory.reads = 0;
    network.service(&mut memory, &mut link);
    assert_eq!(memory.reads, 1); // Only RX needs its initial empty check.
    assert_eq!(memory.writes, 0);
    assert!(network.bridge.guest.front().is_some());
    memory.reads = 0;
    for _ in 0..1000 {
        network.service(&mut memory, &mut link);
    }
    assert_eq!(memory.reads, 0); // Both empty queues now wait for a kick.
    assert_eq!(memory.writes, 0);
}

#[test]
fn a_single_kick_keeps_tx_runnable_across_the_service_budget() {
    let mut network = Network::new();
    configure(&mut network.device);
    let mut memory = Memory::new();
    let frame = frame();
    memory.tx(&frame);
    network.device.write(0x30, 4, 1).unwrap();
    network.device.write(0x44, 4, 0).unwrap();
    network.device.write(0x38, 4, 32).unwrap();
    network.device.write(0x44, 4, 1).unwrap();
    for index in 0..24 {
        memory.descriptor(1, index, RAM + 0x7000, frame.len() + 12, 0, 0);
        memory.put16(0x4004 + index * 2, index as u16);
    }
    memory.put16(0x4002, 24);
    network.device.write(0x50, 4, 1).unwrap();
    let mut link = Link {
        up: true,
        input: vec![],
        output: vec![],
        busy: false,
    };
    network.service(&mut memory, &mut link);
    assert_eq!(memory.get16(0x5002), 16);
    network.service(&mut memory, &mut link);
    assert_eq!(memory.get16(0x5002), 24);
    assert_eq!(link.output, vec![frame; 24]);
    let reads = memory.reads;
    network.service(&mut memory, &mut link);
    assert_eq!(memory.reads, reads);
}
