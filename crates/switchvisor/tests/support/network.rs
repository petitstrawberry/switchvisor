#![allow(dead_code)]
use switchvisor::{
    net::{FRAME_SIZE, GUEST_MAC, HOST_MAC},
    vdev::{
        VirtualDevice,
        virtio_net::{GuestMemory, MemoryError, Net},
    },
};
pub const RAM: u64 = 0x9000_0000;
pub struct Memory {
    pub bytes: Vec<u8>,
    pub reads: usize,
    pub writes: usize,
}
impl Memory {
    pub fn new() -> Self {
        Self {
            bytes: vec![0; 0x20000],
            reads: 0,
            writes: 0,
        }
    }
    pub fn put16(&mut self, at: usize, value: u16) {
        self.bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    pub fn get16(&self, at: usize) -> u16 {
        u16::from_le_bytes(self.bytes[at..at + 2].try_into().unwrap())
    }
    pub fn descriptor(
        &mut self,
        queue: usize,
        index: usize,
        address: u64,
        length: usize,
        flags: u16,
        next: u16,
    ) {
        let at = queue * 0x3000 + index * 16;
        self.bytes[at..at + 8].copy_from_slice(&address.to_le_bytes());
        self.bytes[at + 8..at + 12].copy_from_slice(&(length as u32).to_le_bytes());
        self.put16(at + 12, flags);
        self.put16(at + 14, next);
    }
    pub fn post(&mut self, queue: usize, head: u16) {
        let at = queue * 0x3000 + 0x1000;
        let index = self.get16(at + 2);
        self.put16(at + 4 + usize::from(index % 8) * 2, head);
        self.put16(at + 2, index.wrapping_add(1));
    }
    pub fn tx(&mut self, frame: &[u8]) {
        self.bytes[0x7000..0x700c].fill(0);
        self.bytes[0x700c..0x700c + frame.len()].copy_from_slice(frame);
        self.descriptor(1, 0, RAM + 0x7000, frame.len() + 12, 0, 0);
        self.post(1, 0);
    }
    pub fn rx(&mut self) {
        self.descriptor(0, 0, RAM + 0x8000, FRAME_SIZE + 12, 2, 0);
        self.post(0, 0);
    }
}
impl GuestMemory for Memory {
    fn valid(&self, address: u64, length: usize) -> bool {
        address >= RAM
            && length != 0
            && address
                .checked_add(length as u64)
                .is_some_and(|end| end <= RAM + self.bytes.len() as u64)
    }
    fn read(&mut self, address: u64, out: &mut [u8]) -> Result<(), MemoryError> {
        if !self.valid(address, out.len()) {
            return Err(MemoryError);
        }
        let at = (address - RAM) as usize;
        out.copy_from_slice(&self.bytes[at..at + out.len()]);
        self.reads += 1;
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryError> {
        if !self.valid(address, bytes.len()) {
            return Err(MemoryError);
        }
        let at = (address - RAM) as usize;
        self.bytes[at..at + bytes.len()].copy_from_slice(bytes);
        self.writes += 1;
        Ok(())
    }
    fn barrier(&mut self) {}
}
pub fn configure(net: &mut Net) {
    net.write(0x70, 4, 1).unwrap();
    net.write(0x70, 4, 3).unwrap();
    net.write(0x24, 4, 0).unwrap();
    net.write(0x20, 4, (1 << 5) | (1 << 16)).unwrap();
    net.write(0x24, 4, 1).unwrap();
    net.write(0x20, 4, 1).unwrap();
    net.write(0x70, 4, 11).unwrap();
    for queue in 0..2 {
        net.write(0x30, 4, queue).unwrap();
        net.write(0x38, 4, 8).unwrap();
        net.write(0x80, 4, RAM + queue * 0x3000).unwrap();
        net.write(0x90, 4, RAM + queue * 0x3000 + 0x1000).unwrap();
        net.write(0xa0, 4, RAM + queue * 0x3000 + 0x2000).unwrap();
        net.write(0x44, 4, 1).unwrap();
    }
    net.write(0x70, 4, 15).unwrap();
    assert!(net.running());
}
pub fn frame() -> Vec<u8> {
    let mut bytes = vec![0x5a; 60];
    bytes[..6].copy_from_slice(&HOST_MAC);
    bytes[6..12].copy_from_slice(&GUEST_MAC);
    bytes[12..14].copy_from_slice(&[8, 0]);
    bytes
}
pub fn arp(source: [u8; 6], ip: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![0; 60];
    bytes[..6].fill(255);
    bytes[6..12].copy_from_slice(&source);
    bytes[12..22].copy_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
    bytes[22..28].copy_from_slice(&source);
    bytes[28..32].copy_from_slice(&ip);
    bytes[38..42].copy_from_slice(&switchvisor::net::MANAGEMENT_IP);
    bytes
}
