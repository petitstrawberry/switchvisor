//! Bounded Ethernet switching and a small, static management endpoint.
//! No routing, DHCP, IP forwarding, or guest packet rewriting is performed.

pub const MTU: usize = 1500;
pub const FRAME_SIZE: usize = 14 + MTU;
pub const HOST_MAC: [u8; 6] = [0x02, 0x53, 0x56, 0, 0, 1];
pub const GUEST_MAC: [u8; 6] = [0x02, 0x53, 0x56, 0, 0, 2];
pub const MANAGEMENT_MAC: [u8; 6] = [0x02, 0x53, 0x56, 0, 0, 3];
pub const MANAGEMENT_IP: [u8; 4] = [192, 168, 77, 1];
pub const MANAGEMENT_PORT: u16 = 7777;
const QUEUE_SIZE: usize = 8;

/// Packet operations are atomic: false/zero means retry the entire frame.
pub trait Ethernet {
    fn link_up(&self) -> bool;
    fn receive_frame(&mut self, output: &mut [u8; FRAME_SIZE]) -> usize;
    fn send_frame(&mut self, frame: &[u8]) -> bool;
}

pub struct Frames {
    bytes: [[u8; FRAME_SIZE]; QUEUE_SIZE],
    lengths: [usize; QUEUE_SIZE],
    head: usize,
    count: usize,
}

impl Default for Frames {
    fn default() -> Self {
        Self::new()
    }
}

impl Frames {
    pub const fn new() -> Self {
        Self {
            bytes: [[0; FRAME_SIZE]; QUEUE_SIZE],
            lengths: [0; QUEUE_SIZE],
            head: 0,
            count: 0,
        }
    }
    pub fn full(&self) -> bool {
        self.count == QUEUE_SIZE
    }
    pub fn push(&mut self, frame: &[u8]) -> bool {
        if self.full() || !(14..=FRAME_SIZE).contains(&frame.len()) {
            return false;
        }
        let tail = (self.head + self.count) % QUEUE_SIZE;
        self.bytes[tail][..frame.len()].copy_from_slice(frame);
        self.lengths[tail] = frame.len();
        self.count += 1;
        true
    }
    pub fn front(&self) -> Option<&[u8]> {
        (self.count != 0).then(|| &self.bytes[self.head][..self.lengths[self.head]])
    }
    pub fn pop(&mut self) {
        if self.count != 0 {
            self.head = (self.head + 1) % QUEUE_SIZE;
            self.count -= 1;
        }
    }
    pub fn clear(&mut self) {
        self.count = 0;
        self.head = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    Host,
    Guest,
}

pub struct Bridge {
    pub host: Frames,
    pub guest: Frames,
    pub received: u64,
    pub rejected: u64,
    pub management_replies: u64,
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

impl Bridge {
    pub const fn new() -> Self {
        Self {
            host: Frames::new(),
            guest: Frames::new(),
            received: 0,
            rejected: 0,
            management_replies: 0,
        }
    }

    /// Backpressure guest TX only on host egress. Waiting for guest RX space
    /// here creates a cycle when a guest sends an ACK before recycling RX
    /// buffers. Local management replies may be dropped when guest RX is full,
    /// just like incoming host frames; they must not stall outbound traffic.
    pub fn can_receive_from_guest(&self) -> bool {
        !self.host.full()
    }

    pub fn receive(&mut self, port: Port, frame: &[u8]) -> bool {
        if !(14..=FRAME_SIZE).contains(&frame.len()) || frame[6] & 1 != 0 {
            self.rejected = self.rejected.saturating_add(1);
            return true;
        }
        self.received = self.received.saturating_add(1);
        let local = frame[..6] == MANAGEMENT_MAC;
        let multicast = frame[0] & 1 != 0;
        if local || multicast {
            let mut reply = [0; FRAME_SIZE];
            let length = management(frame, &mut reply);
            if length != 0 {
                let queue = match port {
                    Port::Host => &mut self.host,
                    Port::Guest => &mut self.guest,
                };
                if queue.push(&reply[..length]) {
                    self.management_replies = self.management_replies.saturating_add(1);
                } else {
                    self.rejected = self.rejected.saturating_add(1);
                }
            }
        }
        if !local {
            let queue = match port {
                Port::Host => &mut self.guest,
                Port::Guest => &mut self.host,
            };
            if !queue.push(frame) {
                self.rejected = self.rejected.saturating_add(1);
            }
        }
        true
    }
}

pub struct Network {
    pub device: crate::vdev::virtio_net::Net,
    pub bridge: Bridge,
    link: bool,
}
impl Default for Network {
    fn default() -> Self {
        Self::new()
    }
}
impl Network {
    pub const fn new() -> Self {
        Self {
            device: crate::vdev::virtio_net::Net::new(),
            bridge: Bridge::new(),
            link: false,
        }
    }
    /// Bounded work per exit. A stalled guest may lose Ethernet frames but
    /// cannot prevent host management requests from being serviced.
    pub fn service(
        &mut self,
        memory: &mut impl crate::vdev::virtio_net::GuestMemory,
        ethernet: &mut impl Ethernet,
    ) {
        let up = ethernet.link_up();
        if self.link != up {
            self.bridge.host.clear();
            self.bridge.guest.clear();
            self.link = up;
        }
        self.device.set_link(up);
        if !self.device.running() {
            self.bridge.guest.clear();
        }
        if !up {
            self.bridge.host.clear();
        }
        let mut frame = [0; FRAME_SIZE];
        for _ in 0..16 {
            let mut progressed = false;
            if let Some(bytes) = self.bridge.host.front() {
                if ethernet.send_frame(bytes) {
                    self.bridge.host.pop();
                    progressed = true;
                }
            }
            if let Some(bytes) = self.bridge.guest.front() {
                if self.device.receive(memory, bytes) {
                    self.bridge.guest.pop();
                    progressed = true;
                }
            }
            let length = ethernet.receive_frame(&mut frame);
            if length != 0 {
                self.bridge.receive(Port::Host, &frame[..length]);
                progressed = true;
            }
            if self.bridge.can_receive_from_guest() {
                let length = self.device.transmit(memory, &mut frame);
                if length != 0 {
                    self.bridge.receive(Port::Guest, &frame[..length]);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
    }
}

pub fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in bytes.chunks(2) {
        sum += u32::from(pair[0]) << 8 | u32::from(*pair.get(1).unwrap_or(&0));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

/// Read-only management: ARP, ICMP echo, and UDP `ping` / `status`.
fn management(frame: &[u8], out: &mut [u8; FRAME_SIZE]) -> usize {
    if frame[12..14] == [8, 6] {
        if frame.len() < 42
            || frame[14..22] != [0, 1, 8, 0, 6, 4, 0, 1]
            || frame[38..42] != MANAGEMENT_IP
            || frame[22..28] != frame[6..12]
        {
            return 0;
        }
        out[..6].copy_from_slice(&frame[6..12]);
        out[6..12].copy_from_slice(&MANAGEMENT_MAC);
        out[12..22].copy_from_slice(&frame[12..22]);
        out[21] = 2;
        out[22..28].copy_from_slice(&MANAGEMENT_MAC);
        out[28..32].copy_from_slice(&MANAGEMENT_IP);
        out[32..42].copy_from_slice(&frame[22..32]);
        return 60; // Ethernet minimum, excluding FCS; output was zeroed.
    }
    if frame.len() < 42
        || frame[..6] != MANAGEMENT_MAC
        || frame[12..14] != [8, 0]
        || frame[14] != 0x45
        || frame[30..34] != MANAGEMENT_IP
    {
        return 0;
    }
    let total = usize::from(be16(frame, 16));
    if total < 28
        || total + 14 > frame.len()
        || be16(frame, 20) & 0xbfff != 0
        || frame[22] == 0
        || checksum(&frame[14..34]) != 0
    {
        return 0;
    }
    let payload = &frame[34..14 + total];
    let length = match frame[23] {
        1 if payload[0..2] == [8, 0] && checksum(payload) == 0 => {
            out[34..14 + total].copy_from_slice(payload);
            out[34] = 0;
            out[36..38].fill(0);
            let sum = checksum(&out[34..14 + total]);
            out[36..38].copy_from_slice(&sum.to_be_bytes());
            total
        }
        17 if be16(payload, 2) == MANAGEMENT_PORT
            && usize::from(be16(payload, 4)) == payload.len() =>
        {
            // IPv4 permits a zero UDP checksum. Verify a supplied one, including
            // the pseudo-header, before interpreting even read-only commands.
            if be16(payload, 6) != 0 {
                let mut pseudo = [0u8; FRAME_SIZE + 12];
                pseudo[..8].copy_from_slice(&frame[26..34]);
                pseudo[9] = 17;
                pseudo[10..12].copy_from_slice(&(payload.len() as u16).to_be_bytes());
                pseudo[12..12 + payload.len()].copy_from_slice(payload);
                if checksum(&pseudo[..12 + payload.len()]) != 0 {
                    return 0;
                }
            }
            let response: &[u8] = match &payload[8..] {
                b"ping" | b"ping\n" => b"pong\n",
                b"status" | b"status\n" => {
                    b"switchvisor usb-net management=192.168.77.1 mtu=1500\n"
                }
                _ => return 0,
            };
            out[34..36].copy_from_slice(&MANAGEMENT_PORT.to_be_bytes());
            out[36..38].copy_from_slice(&payload[..2]);
            out[38..40].copy_from_slice(&((8 + response.len()) as u16).to_be_bytes());
            out[42..42 + response.len()].copy_from_slice(response);
            28 + response.len()
        }
        _ => return 0,
    };
    out[..6].copy_from_slice(&frame[6..12]);
    out[6..12].copy_from_slice(&MANAGEMENT_MAC);
    out[12..34].copy_from_slice(&frame[12..34]);
    out[16..18].copy_from_slice(&(length as u16).to_be_bytes());
    out[20..22].fill(0);
    out[22] = 64;
    out[24..26].fill(0);
    out[26..30].copy_from_slice(&MANAGEMENT_IP);
    out[30..34].copy_from_slice(&frame[26..30]);
    let sum = checksum(&out[14..34]);
    out[24..26].copy_from_slice(&sum.to_be_bytes());
    (14 + length).max(60)
}
