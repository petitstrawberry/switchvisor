use switchvisor::{
    loader::{
        Action, BUNDLE_DESCRIPTOR_SIZE, BundleDescriptor, GUEST_RAM_BASE, HEADER_SIZE, Header,
        IMAGE_DESCRIPTOR_SIZE, ImageDescriptor, Loader, MAX_IMAGES, MAX_RESPONSE_SIZE, Opcode,
        PRESERVE_BOOT_ARGS, ProtocolError, REPLY_FLAG, State, StatusCode, Storage, StorageError,
    },
    payload::{STACK_TOP, crc32},
};

const MEMORY_SIZE: usize = 4 * 1024 * 1024;

struct Memory(Vec<u8>);

impl Storage for Memory {
    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), StorageError> {
        let start = address
            .checked_sub(GUEST_RAM_BASE)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(StorageError)?;
        let destination = self
            .0
            .get_mut(start..start + bytes.len())
            .ok_or(StorageError)?;
        destination.copy_from_slice(bytes);
        Ok(())
    }

    fn zero(&mut self, address: u64, length: u64) -> Result<(), StorageError> {
        let start = address
            .checked_sub(GUEST_RAM_BASE)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(StorageError)?;
        let length = usize::try_from(length).map_err(|_| StorageError)?;
        self.0
            .get_mut(start..start + length)
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

fn bundle(entry: u64, image_count: u32) -> BundleDescriptor {
    BundleDescriptor {
        entry,
        image_count,
        flags: PRESERVE_BOOT_ARGS,
        registers: [0; 8],
    }
}

fn image(address: u64, data: &[u8], zero_fill: u64) -> ImageDescriptor {
    ImageDescriptor {
        address,
        file_size: data.len() as u64,
        runtime_size: data.len() as u64 + zero_fill,
        crc32: crc32(data),
        flags: 0,
    }
}

fn begin_bundle(loader: &mut Loader, memory: &mut Memory, descriptor: BundleDescriptor) {
    let mut bytes = [0; BUNDLE_DESCRIPTOR_SIZE];
    descriptor.encode(&mut bytes);
    let (reply, _) = invoke(loader, memory, &request(Opcode::BeginBundle, 1, &bytes));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
}

fn send_image(
    loader: &mut Loader,
    memory: &mut Memory,
    descriptor: ImageDescriptor,
    data: &[u8],
    id: u32,
) {
    let mut bytes = [0; IMAGE_DESCRIPTOR_SIZE];
    descriptor.encode(&mut bytes);
    let (reply, _) = invoke(loader, memory, &request(Opcode::BeginImage, id, &bytes));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
    let split = data.len().min(13);
    for (part, (offset, chunk)) in [(0, &data[..split]), (split as u64, &data[split..])]
        .into_iter()
        .filter(|(_, chunk)| !chunk.is_empty())
        .enumerate()
    {
        let mut header = Header::request(Opcode::Data, id + 1 + part as u32, chunk.len() as u32);
        header.arg0 = offset;
        let (reply, _) = invoke(loader, memory, &message(header, chunk));
        assert_eq!(reply.status_code(), StatusCode::Ok as u64);
        assert_eq!(reply.header.arg1, offset + chunk.len() as u64);
    }
    let (reply, _) = invoke(loader, memory, &request(Opcode::EndImage, id + 3, &[]));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
}

#[test]
fn scatter_upload_commits_all_images_before_boot() {
    let executable = b"payload bytes with an aligned entry";
    let auxiliary = b"opaque auxiliary bytes";
    let first = image(GUEST_RAM_BASE + 0x1000, executable, 32);
    let second = image(GUEST_RAM_BASE + 0x3000, auxiliary, 16);
    let descriptor = bundle(first.address + 4, 2);
    let mut encoded = [0; BUNDLE_DESCRIPTOR_SIZE];
    descriptor.encode(&mut encoded);
    assert_eq!(BundleDescriptor::decode(&encoded), Some(descriptor));
    let mut encoded = [0; IMAGE_DESCRIPTOR_SIZE];
    first.encode(&mut encoded);
    assert_eq!(ImageDescriptor::decode(&encoded), Some(first));

    let mut loader = Loader::new();
    let mut memory = Memory(vec![0xaa; MEMORY_SIZE]);
    let (hello, _) = invoke(&mut loader, &mut memory, &request(Opcode::Hello, 1, &[]));
    assert_eq!(hello.status_code(), StatusCode::Ok as u64);
    assert_eq!(hello.body().len(), 32);

    begin_bundle(&mut loader, &mut memory, descriptor);
    assert_eq!(loader.state(), State::Bundle);
    assert!(loader.claimed());
    send_image(&mut loader, &mut memory, first, executable, 2);
    send_image(&mut loader, &mut memory, second, auxiliary, 6);

    let (commit, action) = invoke(
        &mut loader,
        &mut memory,
        &request(Opcode::CommitBundle, 10, &[]),
    );
    assert_eq!(commit.status_code(), StatusCode::Ok as u64);
    assert_eq!(action, Action::None);
    assert_eq!(loader.state(), State::Ready);

    for (descriptor, data) in [
        (first, executable.as_slice()),
        (second, auxiliary.as_slice()),
    ] {
        let start = (descriptor.address - GUEST_RAM_BASE) as usize;
        assert_eq!(&memory.0[start..start + data.len()], data);
        assert!(
            memory.0[start + data.len()..start + descriptor.runtime_size as usize]
                .iter()
                .all(|byte| *byte == 0)
        );
    }

    let (boot, action) = invoke(&mut loader, &mut memory, &request(Opcode::Boot, 11, &[]));
    assert_eq!(boot.status_code(), StatusCode::Ok as u64);
    assert_eq!(action, Action::Boot(descriptor));
    assert_eq!(loader.state(), State::Disabled);

    let mut response = [0; MAX_RESPONSE_SIZE];
    let length = boot.encode(&mut response);
    let header = Header::decode(&response[..length]).unwrap();
    assert_eq!(header.request_id, 11);
    assert_eq!(header.opcode, Opcode::Boot as u16 | REPLY_FLAG);
}

#[test]
fn incomplete_bad_or_overlapping_images_never_become_bootable() {
    let data = b"abcdefgh";
    let first = image(GUEST_RAM_BASE + 0x1000, data, 32);
    let mut loader = Loader::new();
    let mut memory = Memory(vec![0xaa; MEMORY_SIZE]);
    begin_bundle(&mut loader, &mut memory, bundle(first.address, 2));
    send_image(&mut loader, &mut memory, first, data, 2);

    let (reply, _) = invoke(
        &mut loader,
        &mut memory,
        &request(Opcode::CommitBundle, 6, &[]),
    );
    assert_eq!(reply.status_code(), StatusCode::ImageCount as u64);

    let overlapping = image(first.address + 4, data, 0);
    let mut body = [0; IMAGE_DESCRIPTOR_SIZE];
    overlapping.encode(&mut body);
    let (reply, _) = invoke(
        &mut loader,
        &mut memory,
        &request(Opcode::BeginImage, 7, &body),
    );
    assert_eq!(reply.status_code(), StatusCode::Overlap as u64);

    let second = image(GUEST_RAM_BASE + 0x3000, data, 0);
    let mut bad = second;
    bad.crc32 ^= 1;
    bad.encode(&mut body);
    invoke(
        &mut loader,
        &mut memory,
        &request(Opcode::BeginImage, 8, &body),
    );
    let (reply, _) = invoke(&mut loader, &mut memory, &request(Opcode::Data, 9, data));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
    let (reply, _) = invoke(
        &mut loader,
        &mut memory,
        &request(Opcode::EndImage, 10, &[]),
    );
    assert_eq!(reply.status_code(), StatusCode::Checksum as u64);
    let (reply, action) = invoke(&mut loader, &mut memory, &request(Opcode::Boot, 11, &[]));
    assert_eq!(reply.status_code(), StatusCode::BadState as u64);
    assert_eq!(action, Action::None);
    let (reply, _) = invoke(&mut loader, &mut memory, &request(Opcode::Abort, 12, &[]));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
    assert_eq!(loader.state(), State::Idle);
    assert!(loader.claimed());
}

#[test]
fn descriptors_reject_protected_ranges_and_unuploaded_entries() {
    let data = b"abcdefgh";
    let valid = image(GUEST_RAM_BASE + 0x1000, data, 0);
    for descriptor in [
        bundle(valid.address, 0),
        bundle(valid.address, MAX_IMAGES as u32 + 1),
        bundle(valid.address + 1, 1),
        bundle(STACK_TOP - 4, 1),
        BundleDescriptor {
            registers: [1; 8],
            ..bundle(valid.address, 1)
        },
    ] {
        assert!(!descriptor.validate());
    }
    for descriptor in [
        ImageDescriptor {
            file_size: 0,
            ..valid
        },
        ImageDescriptor {
            runtime_size: valid.file_size - 1,
            ..valid
        },
        ImageDescriptor {
            address: STACK_TOP - 4,
            ..valid
        },
        ImageDescriptor { flags: 1, ..valid },
    ] {
        assert!(!descriptor.validate());
    }

    let mut explicit = bundle(valid.address, 1);
    explicit.flags = 0;
    explicit.registers[0] = 1;
    assert!(explicit.validate());

    let mut loader = Loader::new();
    let mut memory = Memory(vec![0; MEMORY_SIZE]);
    begin_bundle(&mut loader, &mut memory, bundle(GUEST_RAM_BASE + 0x2000, 1));
    send_image(&mut loader, &mut memory, valid, data, 2);
    let (reply, _) = invoke(
        &mut loader,
        &mut memory,
        &request(Opcode::CommitBundle, 6, &[]),
    );
    assert_eq!(reply.status_code(), StatusCode::Entry as u64);
}

#[test]
fn framing_and_disabled_loader_fail_closed() {
    let mut loader = Loader::new();
    let mut memory = Memory(vec![0; MEMORY_SIZE]);
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

    loader.disable();
    let descriptor = bundle(GUEST_RAM_BASE + 0x1000, 1);
    let mut body = [0; BUNDLE_DESCRIPTOR_SIZE];
    descriptor.encode(&mut body);
    let (reply, action) = invoke(
        &mut loader,
        &mut memory,
        &request(Opcode::BeginBundle, 2, &body),
    );
    assert_eq!(reply.status_code(), StatusCode::GuestRunning as u64);
    assert_eq!(action, Action::None);
    let (reply, _) = invoke(&mut loader, &mut memory, &request(Opcode::Status, 3, &[]));
    assert_eq!(reply.status_code(), StatusCode::Ok as u64);
    assert_eq!(reply.header.arg1, State::Disabled as u64);
}
