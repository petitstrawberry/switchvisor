//! Run the production XUDC driver against its MMIO and DMA contracts.
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
use switchvisor::vdev::usb_ownership::{self as ownership, CAR, PMC};
use switchvisor::{
    drivers::{
        Clock, DmaBuffer, Driver, Mmio, RxTransport, TxTransport,
        usb::tegra210::{Channel, DEV, DMA_SIZE, Error, Xudc},
    },
    payload::RESIDENT_BASE,
};

const BASE: u64 = RESIDENT_BASE + 0x200000;
const RINGS: [usize; 12] = [
    0x200, 0x300, 0x400, 0x500, 0x600, 0x700, 0x800, 0x900, 0xa00, 0xb00, 0xc00, 0xd00,
];
const EPS: [u32; 12] = [0, 2, 3, 5, 6, 7, 9, 10, 11, 13, 14, 15];
struct State {
    registers: BTreeMap<u64, u32>,
    dma: [u32; DMA_SIZE / 4],
    writes: Vec<(u64, u32)>,
    latest: [u64; 12],
    event: usize,
    cycle: u32,
    time: u64,
    pll_locks: bool,
}
#[derive(Clone)]
struct Mock(Rc<RefCell<State>>);
struct Buffer {
    mock: Mock,
    base: u64,
}
impl Mock {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(State {
            registers: BTreeMap::new(),
            dma: [0; DMA_SIZE / 4],
            writes: vec![],
            latest: [0; 12],
            event: 0,
            cycle: 1,
            time: 10_000,
            pll_locks: true,
        })))
    }
    fn event(&self, mut words: [u32; 4]) {
        let mut state = self.0.borrow_mut();
        let status = state.registers.get(&(DEV + 0x34)).copied().unwrap_or(0);
        state.registers.insert(DEV + 0x34, status | (1 << 4));
        let index = state.event * 4;
        words[3] |= state.cycle;
        state.dma[index..index + 4].copy_from_slice(&words);
        state.event += 1;
        if state.event == 32 {
            state.event = 0;
            state.cycle ^= 1;
        }
    }
    fn setup(&self, kind: u8, request: u8, value: u16, index: u16, length: u16) {
        self.event([
            u32::from(kind) | (u32::from(request) << 8) | (u32::from(value) << 16),
            u32::from(index) | (u32::from(length) << 16),
            1 << 24,
            63 << 10,
        ]);
    }
    fn complete(&self, ep: u32, remaining: u32, code: u32) {
        let ring = EPS.iter().position(|&id| id == ep).unwrap();
        let pointer = self.0.borrow().latest[ring];
        self.event([
            pointer as u32,
            (pointer >> 32) as u32,
            (code << 24) | remaining,
            (32 << 10) | (ep << 16),
        ]);
    }
    fn trb(&self, ep: u32) -> [u32; 4] {
        let state = self.0.borrow();
        let ring = EPS.iter().position(|&id| id == ep).unwrap();
        let offset = (state.latest[ring] - BASE) as usize / 4;
        state.dma[offset..offset + 4].try_into().unwrap()
    }
    fn bytes(&self, offset: usize, length: usize) -> Vec<u8> {
        self.0.borrow().dma[offset / 4..]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .take(length)
            .collect()
    }
    fn put_bytes(&self, offset: usize, bytes: &[u8]) {
        let mut state = self.0.borrow_mut();
        for (index, &byte) in bytes.iter().enumerate() {
            let address = offset + index;
            let shift = (address & 3) * 8;
            let word = &mut state.dma[address / 4];
            *word = (*word & !(0xff << shift)) | (u32::from(byte) << shift);
        }
    }
    fn port(&self, status: u32) {
        self.0.borrow_mut().registers.insert(DEV + 0x3c, status);
        self.event([0, 0, 1 << 24, 34 << 10]);
    }
}
impl Mmio for Mock {
    fn read32(&mut self, address: u64) -> u32 {
        let state = self.0.borrow();
        if address == CAR + 0x52c && state.pll_locks {
            return state.registers.get(&address).copied().unwrap_or(0) | (1 << 31);
        }
        state.registers.get(&address).copied().unwrap_or(0)
    }
    fn write32(&mut self, address: u64, mut value: u32) {
        let mut state = self.0.borrow_mut();
        state.writes.push((address, value));
        if address == PMC + 0x30 {
            let current = state.registers.get(&(PMC + 0x38)).copied().unwrap_or(0);
            state
                .registers
                .insert(PMC + 0x38, current | (1 << (value & 31)));
        }
        if address == CAR + 0xc0 && value & (1 << 30) != 0 && state.pll_locks {
            value |= 1 << 27;
        }
        if address == DEV + 0x58 {
            // Reload suspends endpoint fetching until software clears EP_PAUSE.
            let paused = state.registers.get(&(DEV + 0x54)).copied().unwrap_or(0);
            state.registers.insert(DEV + 0x54, paused | value);
            value = 0;
        }
        if address == DEV + 0x50 {
            let changed = value ^ state.registers.get(&address).copied().unwrap_or(0);
            let old = state.registers.get(&(DEV + 0x5c)).copied().unwrap_or(0);
            state.registers.insert(DEV + 0x5c, old | changed);
        }
        if address == DEV + 0x5c {
            let old = state.registers.get(&address).copied().unwrap_or(0);
            value = old & !value;
        }
        if address == DEV + 0x34 {
            let old = state.registers.get(&address).copied().unwrap_or(0);
            value = old & !value;
        }
        state.registers.insert(address, value);
    }
    fn barrier(&mut self) {}
}
impl Clock for Mock {
    fn now_us(&mut self) -> u64 {
        self.0.borrow().time
    }
    fn delay_us(&mut self, duration: u32) {
        self.0.borrow_mut().time += u64::from(duration);
    }
}
impl DmaBuffer for Buffer {
    fn physical_base(&self) -> u64 {
        self.base
    }
    fn size(&self) -> usize {
        DMA_SIZE
    }
    fn read32(&mut self, offset: usize) -> u32 {
        assert_eq!(offset % 4, 0);
        self.mock.0.borrow().dma[offset / 4]
    }
    fn write32(&mut self, offset: usize, value: u32) {
        assert_eq!(offset % 4, 0);
        let mut state = self.mock.0.borrow_mut();
        let old = state.dma[offset / 4];
        state.dma[offset / 4] = value;
        if (0x1000..0x1400).contains(&offset) && offset % 64 == 0 && old != value {
            let mask = 1 << ((offset - 0x1000) / 64);
            let old = state.registers.get(&(DEV + 0x5c)).copied().unwrap_or(0);
            state.registers.insert(DEV + 0x5c, old | mask);
        }
        for (ring, base) in RINGS.iter().enumerate() {
            if (*base..*base + 240).contains(&offset)
                && offset % 16 == 12
                && matches!((value >> 10) & 63, 1 | 3 | 4)
            {
                state.latest[ring] = BASE + (offset - 12) as u64;
            }
        }
    }
}
type Usb = Xudc<Mock, Buffer>;
fn new() -> (Usb, Mock) {
    let mock = Mock::new();
    let mut usb = Xudc::new(
        mock.clone(),
        Buffer {
            mock: mock.clone(),
            base: BASE,
        },
    );
    usb.initialize().unwrap();
    (usb, mock)
}
fn configured(usb: &mut Usb, mock: &Mock, high: bool) {
    mock.port(1 | (u32::from(if high { 3u8 } else { 1u8 }) << 10) | (1 << 17));
    usb.poll().unwrap();
    mock.setup(0, 9, 1, 0, 0);
    usb.poll().unwrap();
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert!(!usb.connected());
    assert_eq!(usb.send_capacity(), 0);
    mock.setup(0x21, 0x22, 1, 0, 0);
    usb.poll().unwrap();
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert!(usb.connected());
    assert_eq!(usb.send_capacity(), 512);
}

#[test]
fn initialization_uses_resident_dram_and_enables_physical_usb_irq() {
    let (usb, mock) = new();
    let state = mock.0.borrow();
    assert!(!usb.connected());
    assert_ne!(state.registers[&(DEV + 0x30)] & (1 << 4), 0);
    assert_eq!(state.registers[&(DEV + 0x9188)] & (1 << 16), 0);
    assert_eq!(state.registers[&(DEV + 0x40)], (BASE + 0x1000) as u32);
    assert_eq!(state.registers[&ownership::DEV_ASID], 0);
    // Device attachment requires the override's value, not just its enable.
    let vbus_id = state.registers[&0x7009fc60];
    assert_eq!(vbus_id & (3 << 12), 1 << 12);
    assert_eq!(vbus_id & (3 << 16), 1 << 16);
    assert_ne!(vbus_id & (1 << 14), 0);
    assert_eq!(vbus_id & (15 << 18), 8 << 18);
    assert!(
        !state
            .writes
            .iter()
            .any(|&(address, _)| (0x40000000..0x40040000).contains(&address))
    );
}

#[test]
fn boot_snapshot_is_read_only_and_reports_real_event_progress() {
    let (mut usb, mock) = new();
    let writes = mock.0.borrow().writes.len();
    let status = usb.snapshot();
    assert_eq!(status.configuration, 0);
    assert!(!status.dtr);
    assert_eq!(status.ep0_state, 1);
    assert_ne!(status.vbus_id & (1 << 14), 0);
    assert_eq!(mock.0.borrow().writes.len(), writes);
    assert_eq!(usb.statistics.events, 0);
    mock.setup(0x80, 6, 0x100, 0, 8);
    usb.poll().unwrap();
    assert_eq!(usb.statistics.events, 1);
    assert_eq!(usb.statistics.setups, 1);
    assert_eq!((usb.statistics.last_event[3] >> 10) & 63, 63);
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert_eq!(usb.statistics.events, 2);
    assert_eq!(usb.statistics.setups, 1);
}

#[test]
fn bus_reset_unpauses_ep0_before_the_first_set_address_status() {
    let (mut usb, mock) = new();
    mock.port(1 | (3 << 10) | (1 << 17) | (1 << 21));
    usb.poll().unwrap();
    assert_eq!(mock.0.borrow().registers[&(DEV + 0x54)] & 1, 0);
    // Raw SET_ADDRESS event captured on Switch hardware (IMG_9110).
    mock.event([0x00020500, 0, 0x01000000, 0x0000fc30]);
    usb.poll().unwrap();
    assert_eq!((mock.trb(0)[3] >> 10) & 63, 4);
    assert_ne!(mock.trb(0)[3] & (1 << 16), 0);
    assert_eq!(mock.0.borrow().registers[&(DEV + 4)], 0);
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert_eq!(usb.statistics.events, 3);
    mock.setup(0x80, 6, 0x100, 0, 18);
    usb.poll().unwrap();
    assert_eq!(mock.trb(0)[2], 18);
}

#[test]
fn host_enumeration_descriptors_address_and_control_stages_work() {
    let (mut usb, mock) = new();
    mock.setup(0x80, 6, 0x100, 0, 8);
    usb.poll().unwrap();
    assert_eq!(mock.bytes(0x1400, 8), [18, 1, 0, 2, 0xef, 2, 1, 64]);
    assert_eq!(mock.trb(0)[2], 8);
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert_eq!((mock.trb(0)[3] >> 10) & 63, 4);
    assert_eq!(mock.trb(0)[3] & (1 << 16), 0);
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    mock.setup(0, 5, 42, 0, 0);
    usb.poll().unwrap();
    assert_eq!(mock.0.borrow().registers[&(DEV + 0x30)] >> 24, 0x80 | 42);
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    configured(&mut usb, &mock, true);
    mock.setup(0x80, 6, 0x200, 0, 255);
    usb.poll().unwrap();
    let bytes = mock.bytes(0x1400, 164);
    assert_eq!(&bytes[..9], &[9, 2, 164, 0, 5, 1, 0, 0xc0, 1]);
    let mut endpoints = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let length = usize::from(bytes[cursor]);
        assert!(length >= 2 && cursor + length <= bytes.len());
        if bytes[cursor + 1] == 5 {
            endpoints.push(bytes[cursor + 2]);
        }
        cursor += length;
    }
    assert_eq!(endpoints, [0x82, 0x01, 0x81, 0x84, 0x03, 0x83, 0x05, 0x85]);
    assert_eq!(mock.0.borrow().dma[(0x1000 + 3 * 64 + 4) / 4] >> 16, 512);
}

#[test]
fn gdb_profile_adds_only_its_own_cdc_endpoints_and_channel() {
    let mock = Mock::new();
    let mut usb = Xudc::new(
        mock.clone(),
        Buffer {
            mock: mock.clone(),
            base: BASE,
        },
    );
    usb.set_gdb_enabled(true);
    usb.initialize().unwrap();
    configured(&mut usb, &mock, true);
    mock.setup(0x80, 6, 0x200, 0, 255);
    usb.poll().unwrap();
    let descriptor = mock.bytes(0x1400, 230);
    assert_eq!(&descriptor[..9], &[9, 2, 230, 0, 7, 1, 0, 0xc0, 1]);
    let mut endpoints = Vec::new();
    let mut cursor = 0;
    while cursor < descriptor.len() {
        let length = usize::from(descriptor[cursor]);
        assert!(length >= 2 && cursor + length <= descriptor.len());
        if descriptor[cursor + 1] == 5 {
            endpoints.push(descriptor[cursor + 2]);
        }
        cursor += length;
    }
    assert_eq!(
        endpoints,
        [
            0x82, 0x01, 0x81, 0x84, 0x03, 0x83, 0x05, 0x85, 0x86, 0x07, 0x87
        ]
    );
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    mock.setup(0x21, 0x22, 1, 5, 0);
    usb.poll().unwrap();
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert!(usb.connected_channel(Channel::Gdb));
    assert_eq!(usb.send_channel(Channel::Gdb, b"$S05#b8"), Ok(7));
    assert_eq!(mock.bytes(0x2400, 7), b"$S05#b8");
    mock.put_bytes(0x2200, b"$?#3f");
    mock.complete(14, 512 - 5, 13);
    usb.poll().unwrap();
    let mut received = [0; 8];
    assert_eq!(usb.receive_channel(Channel::Gdb, &mut received), Ok(5));
    assert_eq!(&received[..5], b"$?#3f");
}

#[test]
fn composite_channels_have_independent_connection_and_storage() {
    let (mut usb, mock) = new();
    configured(&mut usb, &mock, true);
    assert!(!usb.connected_channel(Channel::Control));
    assert!(usb.connected_channel(Channel::Loader));

    mock.setup(0x21, 0x22, 1, 2, 0);
    usb.poll().unwrap();
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert!(usb.connected_channel(Channel::Control));

    assert_eq!(usb.send_channel(Channel::Control, b"status\n"), Ok(7));
    assert_eq!(mock.bytes(0x1e00, 7), b"status\n");
    assert_eq!(usb.send_channel(Channel::Loader, b"reply"), Ok(5));
    assert_eq!(mock.bytes(0x4000, 5), b"reply");

    let control_input = b"ping\n";
    mock.put_bytes(0x1c00, control_input);
    mock.complete(6, 512 - control_input.len() as u32, 13);
    usb.poll().unwrap();
    let mut received = [0; 16];
    let count = usb
        .receive_channel(Channel::Control, &mut received)
        .unwrap();
    assert_eq!(&received[..count], control_input);

    let loader_input = b"loader frame";
    mock.put_bytes(0x3000, loader_input);
    mock.complete(10, 4096 - loader_input.len() as u32, 13);
    usb.poll().unwrap();
    let count = usb.receive_channel(Channel::Loader, &mut received).unwrap();
    assert_eq!(&received[..count], loader_input);
}

#[test]
fn loader_out_accepts_a_full_message_buffer_and_rearms() {
    let (mut usb, mock) = new();
    configured(&mut usb, &mock, true);
    let input: Vec<u8> = (0..4096).map(|index| index as u8).collect();
    mock.put_bytes(0x3000, &input);
    let previous = mock.0.borrow().latest[7];
    mock.complete(10, 0, 1);
    usb.poll().unwrap();

    let mut output = vec![0; 4096];
    assert_eq!(
        usb.receive_channel(Channel::Loader, &mut output),
        Ok(output.len())
    );
    assert_eq!(output, input);
    assert_ne!(mock.0.borrow().latest[7], previous);
    assert_eq!(mock.trb(10)[2], 4096);
}

#[test]
fn bulk_output_is_nonblocking_and_exact_packets_get_a_zlp() {
    for high in [false, true] {
        let (mut usb, mock) = new();
        assert_eq!(usb.send_capacity(), 0);
        assert_eq!(usb.send(b"before host"), Ok(0));
        configured(&mut usb, &mock, high);
        let bytes = vec![0x5a; if high { 512 } else { 64 }];
        assert_eq!(usb.send(&bytes), Ok(bytes.len()));
        assert_eq!(usb.send_capacity(), 0);
        assert_eq!(mock.bytes(0x1800, bytes.len()), bytes);
        assert_eq!(usb.send(b"not idle\n"), Ok(0));
        mock.complete(3, 0, 1);
        usb.poll().unwrap();
        assert_eq!(mock.trb(3)[2], 0);
        assert_eq!(usb.send_capacity(), 0);
        assert_eq!(usb.send(b"zlp pending\n"), Ok(0));
        mock.complete(3, 0, 1);
        usb.poll().unwrap();
        assert_eq!(usb.send_capacity(), 512);
        assert_eq!(usb.send(b"next\n"), Ok(5));
        mock.complete(3, 0, 1);
        usb.poll().unwrap();
        assert_eq!(usb.statistics.transmitted, bytes.len() as u64 + 5);
    }
}

#[test]
fn closing_and_reopening_the_host_port_preserves_pending_tx_storage() {
    let (mut usb, mock) = new();
    configured(&mut usb, &mock, true);
    assert_eq!(usb.send(b"pending\n"), Ok(8));
    let pointer = mock.0.borrow().latest[2];

    mock.setup(0x21, 0x22, 0, 0, 0);
    usb.poll().unwrap();
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert!(!usb.connected());
    assert_eq!(usb.send_capacity(), 0);
    for _ in 0..1000 {
        assert_eq!(usb.send(b"blocked\n"), Ok(0));
    }
    assert_eq!(mock.0.borrow().latest[2], pointer);
    assert_eq!(mock.bytes(0x1800, 8), b"pending\n");

    mock.complete(3, 0, 1);
    usb.poll().unwrap();
    assert_eq!(usb.send_capacity(), 0);
    mock.setup(0x21, 0x22, 1, 0, 0);
    usb.poll().unwrap();
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    assert_eq!(usb.send_capacity(), 512);
    assert_eq!(usb.send(b"resumed\n"), Ok(8));
    assert_eq!(mock.bytes(0x1800, 8), b"resumed\n");
}

#[test]
fn line_coding_is_metadata_and_host_input_waits_for_transport_rx() {
    let (mut usb, mock) = new();
    configured(&mut usb, &mock, true);
    mock.setup(0x21, 0x20, 0, 0, 7);
    usb.poll().unwrap();
    assert_eq!(mock.trb(0)[3] & (1 << 16), 0);
    let coding = [0x80, 0x25, 0, 0, 0, 0, 8];
    {
        let mut state = mock.0.borrow_mut();
        state.dma[0x1400 / 4] = u32::from_le_bytes(coding[..4].try_into().unwrap());
        state.dma[0x1404 / 4] = u32::from_le_bytes([0, 0, 8, 0]);
    }
    mock.complete(0, 0, 13);
    usb.poll().unwrap();
    assert_ne!(mock.trb(0)[3] & (1 << 16), 0);
    mock.complete(0, 0, 1);
    usb.poll().unwrap();
    mock.setup(0xa1, 0x21, 0, 0, 7);
    usb.poll().unwrap();
    assert_eq!(mock.bytes(0x1400, 7), coding);
    let input = b"host input\n";
    mock.put_bytes(0x1600, input);
    let previous = mock.0.borrow().latest[1];
    mock.complete(2, 512 - input.len() as u32, 13);
    usb.poll().unwrap();
    assert_eq!(mock.0.borrow().latest[1], previous);
    assert_eq!(usb.statistics.received, input.len() as u64);

    let mut first = [0; 4];
    assert_eq!(usb.receive(&mut first), Ok(first.len()));
    assert_eq!(&first, &input[..first.len()]);
    assert_eq!(mock.0.borrow().latest[1], previous);

    let mut rest = [0; 32];
    let count = usb.receive(&mut rest).unwrap();
    assert_eq!(&rest[..count], &input[first.len()..]);
    assert_ne!(mock.0.borrow().latest[1], previous);
    assert_eq!(mock.trb(2)[2], 512);
    assert_eq!(usb.receive(&mut rest), Ok(0));
}

#[test]
fn unsupported_control_requests_stall_and_the_next_setup_clears_ep0() {
    let (mut usb, mock) = new();
    mock.setup(0xc0, 0xff, 0, 0, 64);
    usb.poll().unwrap();
    assert_ne!(mock.0.borrow().registers[&(DEV + 0x50)] & 1, 0);
    mock.setup(0x80, 6, 0x100, 0, 18);
    usb.poll().unwrap();
    assert_eq!(mock.0.borrow().registers[&(DEV + 0x50)] & 1, 0);
    assert_eq!(mock.trb(0)[2], 18);
}

#[test]
fn control_and_event_rings_wrap_without_reusing_owned_trbs() {
    let (mut usb, mock) = new();
    for _ in 0..40 {
        mock.setup(0x80, 8, 0, 0, 1);
        usb.poll().unwrap();
        mock.complete(0, 0, 1);
        usb.poll().unwrap();
        mock.complete(0, 0, 1);
        usb.poll().unwrap();
    }
    assert_eq!(usb.statistics.errors, 0);
    let state = mock.0.borrow();
    assert_eq!(
        state.registers[&(DEV + 0x20)] & !15,
        (BASE + 24 * 16) as u32
    );
}

#[test]
fn unplug_and_bus_reset_drop_inflight_bytes_and_allow_reenumeration() {
    let (mut usb, mock) = new();
    configured(&mut usb, &mock, true);
    assert_eq!(usb.send(b"unplug\n"), Ok(7));
    mock.port(1 << 17);
    usb.poll().unwrap();
    assert!(!usb.connected());
    assert_eq!(usb.statistics.dropped, 7);
    assert_eq!(usb.send(b"not connected\n"), Ok(0));
    configured(&mut usb, &mock, true);
    assert_eq!(usb.send(b"again\n"), Ok(6));
    mock.complete(3, 0, 4);
    assert_eq!(usb.poll(), Err(Error::Transfer));
    assert!(!usb.connected());
    mock.port(1 | (3 << 10) | (1 << 21));
    usb.poll().unwrap();
    configured(&mut usb, &mock, true);
    assert_eq!(usb.send(b"recovered\n"), Ok(10));
}

#[test]
fn invalid_dma_or_clock_timeout_fails_before_touching_guest_storage() {
    for base in [0, 0xaa000000, BASE + 1, RESIDENT_BASE + 0x1000000 - 4096] {
        let mock = Mock::new();
        let mut usb = Xudc::new(
            mock.clone(),
            Buffer {
                mock: mock.clone(),
                base,
            },
        );
        assert_eq!(usb.initialize(), Err(Error::DmaRegion));
        assert!(mock.0.borrow().writes.is_empty());
    }
    let mock = Mock::new();
    mock.0.borrow_mut().pll_locks = false;
    let mut usb = Xudc::new(
        mock.clone(),
        Buffer {
            mock: mock.clone(),
            base: BASE,
        },
    );
    assert_eq!(usb.initialize(), Err(Error::Timeout));
    assert!(mock.0.borrow().time < 12000);
}

#[test]
fn guest_clock_writes_preserve_only_usb_bits_and_write_one_semantics() {
    let bit = (1 << 31) | (1 << 25);
    assert_eq!(ownership::write(CAR + 0x18, 0, bit | 4), Some(bit));
    assert_eq!(ownership::write(CAR + 0x18, u32::MAX, 0), Some(!bit));
    for offset in [0x310, 0x314, 0x330, 0x334] {
        assert_eq!(ownership::write(CAR + offset, u32::MAX, bit), Some(!bit));
        assert!(!ownership::reads_current(CAR + offset));
    }
    assert_eq!(ownership::write(CAR + 0xc0, 0, u32::MAX), None);
    assert_eq!(ownership::write(PMC + 0xf0, 0, 12), Some(12));
    assert_eq!(ownership::write(PMC + 0x30, 0x100 | 21, 0), None);
    assert_eq!(
        ownership::write(PMC + 0x30, 0x100 | 12, 0),
        Some(0x100 | 12)
    );
    assert_eq!(ownership::write(ownership::DEV_ASID, 0x80000001, 0), None);
    assert_eq!(ownership::write(CAR + 0x150, 0x42, 0), Some(0x42));
}
