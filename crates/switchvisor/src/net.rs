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

/// A received frame is borrowed until the callback returns, then consumed.
/// A failed send leaves ownership with the caller; retry the entire frame.
pub trait Ethernet {
    fn link_up(&self) -> bool;
    fn receive_frame(&mut self, receive: impl FnOnce(&[u8])) -> bool;
    fn send_frame(&mut self, frame: &[u8]) -> bool;
    /// Consume only accepted frames, preserving order across backpressure.
    /// A transport may combine queued frames into one hardware transfer.
    fn send_frames(&mut self, frames: &mut Frames) -> usize {
        if let Some(frame) = frames.front() {
            if self.send_frame(frame) {
                frames.pop();
                return 1;
            }
        }
        0
    }
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
    pub fn empty(&self) -> bool {
        self.count == 0
    }
    /// Reserve the tail in place. It stays invisible until `commit` succeeds;
    /// an empty/invalid packet can simply abandon the reservation.
    fn spare(&mut self) -> Option<&mut [u8; FRAME_SIZE]> {
        if self.full() {
            return None;
        }
        Some(&mut self.bytes[(self.head + self.count) % QUEUE_SIZE])
    }
    fn commit(&mut self, length: usize) -> bool {
        if self.full() || !(14..=FRAME_SIZE).contains(&length) {
            return false;
        }
        self.lengths[(self.head + self.count) % QUEUE_SIZE] = length;
        self.count += 1;
        true
    }
    pub fn push(&mut self, frame: &[u8]) -> bool {
        if self.full() || !(14..=FRAME_SIZE).contains(&frame.len()) {
            return false;
        }
        self.spare().unwrap()[..frame.len()].copy_from_slice(frame);
        self.commit(frame.len())
    }
    pub fn front(&self) -> Option<&[u8]> {
        (self.count != 0).then(|| &self.bytes[self.head][..self.lengths[self.head]])
    }
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.count).map(|offset| {
            let index = (self.head + offset) % QUEUE_SIZE;
            &self.bytes[index][..self.lengths[index]]
        })
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
        let (reply, forward) = match port {
            Port::Host => (&mut self.host, &mut self.guest),
            Port::Guest => (&mut self.guest, &mut self.host),
        };
        if Self::route(
            frame,
            reply,
            &mut self.received,
            &mut self.rejected,
            &mut self.management_replies,
        ) && !forward.push(frame)
        {
            self.rejected = self.rejected.saturating_add(1);
        }
        true
    }

    // Classify and generate replies directly into their egress ring slot.
    fn route(
        frame: &[u8],
        reply: &mut Frames,
        received: &mut u64,
        rejected: &mut u64,
        replies: &mut u64,
    ) -> bool {
        if !(14..=FRAME_SIZE).contains(&frame.len()) || frame[6] & 1 != 0 {
            *rejected = rejected.saturating_add(1);
            return false;
        }
        *received = received.saturating_add(1);
        let local = frame[..6] == MANAGEMENT_MAC;
        if local || frame[0] & 1 != 0 {
            if let Some(output) = reply.spare() {
                let length = management(frame, output);
                if reply.commit(length) {
                    *replies = replies.saturating_add(1);
                }
            } else {
                // Management must never stall forwarding or a guest's TX ACK.
                *rejected = rejected.saturating_add(1);
            }
        }
        !local
    }

    fn receive_host(&mut self, frame: &[u8], deliver: impl FnOnce(&[u8]) -> bool) {
        if Self::route(
            frame,
            &mut self.host,
            &mut self.received,
            &mut self.rejected,
            &mut self.management_replies,
        ) && !(self.guest.empty() && deliver(frame))
            && !self.guest.push(frame)
        {
            self.rejected = self.rejected.saturating_add(1);
        }
    }

    fn receive_guest(&mut self, fill: impl FnOnce(&mut [u8; FRAME_SIZE]) -> usize) -> bool {
        let Some(slot) = self.host.spare() else {
            return false;
        };
        let length = fill(slot);
        if length == 0 {
            return false;
        }
        if Self::route(
            &slot[..length],
            &mut self.guest,
            &mut self.received,
            &mut self.rejected,
            &mut self.management_replies,
        ) {
            self.host.commit(length);
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
        for _ in 0..16 {
            let mut progressed = false;
            if ethernet.send_frames(&mut self.bridge.host) != 0 {
                progressed = true;
            }
            if let Some(bytes) = self.bridge.guest.front() {
                if self.device.receive(memory, bytes) {
                    self.bridge.guest.pop();
                    progressed = true;
                }
            }
            if ethernet.receive_frame(|frame| {
                self.bridge
                    .receive_host(frame, |bytes| self.device.receive(memory, bytes));
            }) {
                progressed = true;
            }
            if self
                .bridge
                .receive_guest(|slot| self.device.transmit(memory, slot))
            {
                progressed = true;
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
        out[42..60].fill(0); // Reused ring slots must not leak old padding.
        return 60; // Ethernet minimum, excluding FCS.
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
                // Even-length pseudo-header components can be summed without
                // copying the UDP payload into a second full-size packet.
                let mut sum = u32::from(!checksum(&frame[26..34]))
                    + 17
                    + payload.len() as u32
                    + u32::from(!checksum(payload));
                while sum >> 16 != 0 {
                    sum = (sum & 0xffff) + (sum >> 16);
                }
                if sum != 0xffff {
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
            out[40..42].fill(0); // IPv4 permits an omitted UDP checksum.
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
    let end = (14 + length).max(60);
    out[14 + length..end].fill(0);
    end
}
