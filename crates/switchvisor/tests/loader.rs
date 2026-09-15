use switchvisor::{
    loader::{
        Action, DESCRIPTOR_SIZE, Descriptor, HEADER_SIZE, Header, Loader, MAX_RESPONSE_SIZE,
        Opcode, PRESERVE_BOOT_ARGS, ProtocolError, REPLY_FLAG, State, StatusCode, Storage,
        StorageError,
    },
    payload::crc32,
};

struct Memory(Vec<u8>);

impl Storage for Memory {
    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), StorageError> {
        let start = offset as usize;
        let destination = self
            .0
            .get_mut(start..start + bytes.len())
            .ok_or(StorageError)?;
        destination.copy_from_slice(bytes);
        Ok(())
    }

    fn zero(&mut self, offset: u64, length: u64) -> Result<(), StorageError> {
        self.0
            .get_mut(offset as usize..(offset + length) as usize)
            .ok_or(StorageError)?
            .fill(0);
        Ok(())
    }
}

fn message(mut header: Header, body: &[u8]) -> Vec<u8> {
    header.length = body.len() as u32;
    let mut encoded = [0; HEADER_SIZE];
    header.encode(&mut encoded);
    let mut bytes = encoded.to_vec();
    bytes.extend_from_slice(body);
    bytes
}

fn request(opcode: Opcode, id: u32, body: &[u8]) -> Vec<u8> {
    message(Header::request(opcode, id, body.len() as u32), body)
}

fn invoke(
    loader: &mut Loader,
    memory: &mut Memory,
    bytes: &[u8],
) -> (switchvisor::loader::Response, Action) {
    loader.handle(bytes, memory).unwrap()
}

fn descriptor(data: &[u8]) -> Descriptor {
    Descriptor {
        file_size: data.len() as u64,
        runtime_size: data.len() as u64 + 32,
        entry_offset: 4,
        flags: PRESERVE_BOOT_ARGS,
        crc32: crc32(data),
        registers: [0; 8],
    }
}

#[test]
fn sequential_upload_commit_and_boot_follow_the_raw_payload_contract() {
    let data = b"payload bytes with an aligned entry";
    let descriptor = descriptor(data);
    let mut encoded_descriptor = [0; DESCRIPTOR_SIZE];
    descriptor.encode(&mut encoded_descriptor);
    assert_eq!(Descriptor::decode(&encoded_descriptor), Some(descriptor));

    let mut loader = Loader::new();
    let mut memory = Memory(vec![0xaa; descriptor.runtime_size as usize]);
    let (hello, _) = invoke(&mut loader, &mut memory, &request(Opcode::Hello, 1, &[]));
    assert_eq!(hello.status_code(), StatusCode::Ok as u64);
    assert_eq!(hello.body().len(), 24);

    let (begin, _) = invoke(
        &mut loader,
        &mut memory,
        &request(Opcode::Begin, 2, &encoded_descriptor),
    );
    assert_eq!(begin.header.request_id, 2);
    assert_eq!(begin.header.opcode, Opcode::Begin as u16 | REPLY_FLAG);
    assert_eq!(loader.state(), State::Receiving);
    assert!(loader.claimed());

    let split = 13;
    for (id, (offset, chunk)) in [(0, &data[..split][..]), (split as u64, &data[split..][..])]
        .into_iter()
        .enumerate()
    {
        let mut header = Header::request(Opcode::Data, 3 + id as u32, chunk.len() as u32);
        header.arg0 = offset;
        let (reply, _) = invoke(&mut loader, &mut memory, &message(header, chunk));
        assert_eq!(reply.status_code(), StatusCode::Ok as u64);
    }
    assert_eq!(loader.received(), data.len() as u64);

    let (commit, action) = invoke(&mut loader, &mut memory, &request(Opcode::Commit, 5, &[]));
    assert_eq!(commit.status_code(), StatusCode::Ok as u64);
    assert_eq!(action, Action::None);
    assert_eq!(loader.state(), State::Ready);
    assert_eq!(&memory.0[..data.len()], data);
    assert!(memory.0[data.len()..].iter().all(|byte| *byte == 0));

    let (boot, action) = invoke(&mut loader, &mut memory, &request(Opcode::Boot, 6, &[]));
    assert_eq!(boot.status_code(), StatusCode::Ok as u64);
    assert_eq!(action, Action::Boot(descriptor));
    assert_eq!(loader.state(), State::Disabled);

    let mut encoded = [0; MAX_RESPONSE_SIZE];
    let length = boot.encode(&mut encoded);
    let header = Header::decode(&encoded[..length]).unwrap();
    assert_eq!(header.request_id, 6);
    assert_eq!(header.opcode, Opcode::Boot as u16 | REPLY_FLAG);
}

#[test]
fn transfer_errors_never_make_an_invalid_payload_bootable() {
    let data = b"abcdefgh";
    let mut descriptor = descriptor(data);
    descriptor.entry_offset = 0;
    descriptor.crc32 ^= 1;
    let mut body = [0; DESCRIPTOR_SIZE];
    descriptor.encode(&mut body);
    let mut loader = Loader::new();
    let mut memory = Memory(vec![0xaa; descriptor.runtime_size as usize]);
    invoke(&mut loader, &mut memory, &request(Opcode::Begin, 1, &body));

    let mut wrong = Header::request(Opcode::Data, 2, data.len() as u32);
    wrong.arg0 = 1;
    let (reply, _) = invoke(&mut loader, &mut memory, &message(wrong, data));
    assert_eq!(reply.status_code(), StatusCode::BadOffset as u64);
    assert_eq!(loader.received(), 0);

    let (reply, _) = invoke(&mut loader, &mut memory, &request(Opcode::Data, 3, data));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
    let (reply, _) = invoke(&mut loader, &mut memory, &request(Opcode::Commit, 4, &[]));
    assert_eq!(reply.status_code(), StatusCode::Checksum as u64);
    assert_eq!(loader.state(), State::Receiving);
    let (reply, action) = invoke(&mut loader, &mut memory, &request(Opcode::Boot, 5, &[]));
    assert_eq!(reply.status_code(), StatusCode::BadState as u64);
    assert_eq!(action, Action::None);
    let (reply, _) = invoke(&mut loader, &mut memory, &request(Opcode::Abort, 6, &[]));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
    assert_eq!(loader.state(), State::Idle);
    assert!(loader.claimed());
}

#[test]
fn framing_and_descriptor_validation_reject_malformed_input() {
    let mut loader = Loader::new();
    let mut memory = Memory(vec![0; 128]);
    assert_eq!(
        loader.handle(b"short", &mut memory),
        Err(ProtocolError::Truncated)
    );
    let mut invalid = request(Opcode::Hello, 1, &[]);
    invalid[0] ^= 1;
    assert_eq!(
        loader.handle(&invalid, &mut memory),
        Err(ProtocolError::Magic)
    );
    let mut descriptor = descriptor(b"12345678");
    for mutate in [
        |value: &mut Descriptor| value.file_size = 0,
        |value: &mut Descriptor| value.runtime_size = value.file_size - 1,
        |value: &mut Descriptor| value.entry_offset = 1,
        |value: &mut Descriptor| value.flags = 2,
        |value: &mut Descriptor| value.registers[0] = 1,
    ] {
        let mut value = descriptor;
        mutate(&mut value);
        let mut body = [0; DESCRIPTOR_SIZE];
        value.encode(&mut body);
        let (reply, _) = invoke(&mut loader, &mut memory, &request(Opcode::Begin, 2, &body));
        assert_eq!(reply.status_code(), StatusCode::InvalidDescriptor as u64);
    }
    descriptor.flags = 0;
    descriptor.registers[0] = 1;
    assert!(descriptor.validate());
}

#[test]
fn disabled_loader_reports_guest_running_and_keeps_status_available() {
    let mut loader = Loader::new();
    loader.disable();
    let mut memory = Memory(vec![]);
    let body = [0; DESCRIPTOR_SIZE];
    let (reply, action) = invoke(&mut loader, &mut memory, &request(Opcode::Begin, 1, &body));
    assert_eq!(reply.status_code(), StatusCode::GuestRunning as u64);
    assert_eq!(action, Action::None);
    let (reply, _) = invoke(&mut loader, &mut memory, &request(Opcode::Status, 2, &[]));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
    assert_eq!(reply.header.arg1, State::Disabled as u64);
}
