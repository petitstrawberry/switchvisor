#[path = "support/network.rs"]
mod support;
use support::*;
use switchvisor::net::*;

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
    fn receive_frame(&mut self, out: &mut [u8; FRAME_SIZE]) -> usize {
        if self.input.is_empty() {
            return 0;
        }
        let frame = self.input.remove(0);
        out[..frame.len()].copy_from_slice(&frame);
        frame.len()
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
    assert_eq!(memory.reads, 2); // No RX buffers and no TX packets: stop immediately.
    assert_eq!(memory.writes, 0);
    assert!(network.bridge.guest.front().is_some());
}
