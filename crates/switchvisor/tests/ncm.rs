use switchvisor::drivers::usb::{
    composite::{CONTROL_SIZE, Composite, Reply, Setup},
    ncm,
};

#[test]
fn ntb_roundtrip_supports_minimum_and_maximum_frames() {
    for size in [14, 60, 484, 1514] {
        let frame = vec![0x5a; size];
        let mut bytes = [0; ncm::NTB_SIZE];
        let length = ncm::encode(&frame, u16::MAX, &mut bytes).unwrap();
        let block = ncm::decode(&bytes[..length]).unwrap();
        assert_eq!(block.count, 1);
        // Match a host's NCM payload alignment: the divisor/remainder apply
        // after the 14-byte Ethernet header, not at the datagram start.
        assert_eq!(block.datagrams[0].offset, 30);
        assert_eq!((block.datagrams[0].offset + 14) % 4, 0);
        assert_eq!(block.datagrams[0].length, size);
        assert_eq!(&bytes[block.datagrams[0].offset..length], frame);
        for end in 0..length {
            assert!(ncm::decode(&bytes[..end]).is_err());
        }
    }
}

#[test]
fn parses_multiple_frames_and_rejects_overlaps_cycles_and_unterminated_tables() {
    let mut bytes = [0u8; 160];
    ncm::encode(&[0x33; 60], 0, &mut bytes).unwrap();
    bytes[8..10].copy_from_slice(&160u16.to_le_bytes());
    bytes[16..18].copy_from_slice(&20u16.to_le_bytes());
    bytes.copy_within(30..90, 34);
    bytes[20..22].copy_from_slice(&34u16.to_le_bytes());
    bytes[24..26].copy_from_slice(&98u16.to_le_bytes());
    bytes[26..28].copy_from_slice(&60u16.to_le_bytes());
    bytes[28..32].fill(0);
    bytes[98..158].fill(0x44);
    let block = ncm::decode(&bytes).unwrap();
    assert_eq!(block.count, 2);
    for (offset, value) in [
        (24, 34u16),
        (18, 12),
        (20, 12),
        (16, 16),
        (26, 1514),
        (24, 159),
    ] {
        let mut bad = bytes;
        bad[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        assert!(ncm::decode(&bad).is_err(), "{offset} {value}");
    }
    // Every single-byte mutation is at least safely bounded, whether valid or not.
    for index in 0..bytes.len() {
        for value in [0, 1, 127, 255] {
            let mut bad = bytes;
            bad[index] = value;
            let _ = ncm::decode(&bad);
        }
    }
}

#[test]
fn ncm_descriptors_and_control_requests_are_opt_in() {
    let mut device = Composite::new();
    let mut out = [0; CONTROL_SIZE];
    let setup = |kind, request, value, index, length| Setup {
        request_type: kind,
        request,
        value,
        index,
        length,
    };
    assert_eq!(
        device.setup(setup(0x80, 6, 0x200, 0, 255), &mut out, true),
        Reply::Data(164)
    );
    device.ncm.enabled = true;
    assert_eq!(
        device.setup(setup(0x80, 6, 0x200, 0, 255), &mut out, true),
        Reply::Data(249)
    );
    assert_eq!(out[4], 7);
    device.configuration = 1;
    assert_eq!(
        device.setup(setup(0xa1, 0x80, 0, 5, 28), &mut out, true),
        Reply::Data(28)
    );
    assert_eq!(&out[..4], &[28, 0, 1, 0]);
    assert_eq!(u32::from_le_bytes(out[16..20].try_into().unwrap()), 16384);
    assert_eq!(
        device.setup(setup(1, 11, 1, 6, 0), &mut out, true),
        Reply::NcmAlternate(1)
    );
    assert_eq!(
        device.setup(setup(1, 11, 2, 6, 0), &mut out, true),
        Reply::Stall
    );
    assert_eq!(
        device.setup(setup(0x21, 0x84, 1, 5, 0), &mut out, true),
        Reply::Stall
    );
    assert_eq!(
        device.setup(setup(0x21, 0x86, 0, 5, 4), &mut out, true),
        Reply::NcmInputSize
    );
    device.ncm.alternate = 1;
    device.ncm.packet_filter = 15;
    device.reset();
    assert!(device.ncm.enabled);
    assert!(!device.ncm.active());
    assert_eq!(device.ncm.packet_filter, 0);
}

#[test]
fn chained_ndps_with_host_payload_alignment_are_accepted() {
    let mut bytes = [0u8; 170];
    bytes[..4].copy_from_slice(b"NCMH");
    bytes[4..6].copy_from_slice(&12u16.to_le_bytes());
    bytes[8..10].copy_from_slice(&170u16.to_le_bytes());
    bytes[10..12].copy_from_slice(&12u16.to_le_bytes());
    for (table, next, frame) in [(12usize, 28u16, 46u16), (28, 0, 110)] {
        bytes[table..table + 4].copy_from_slice(b"NCM0");
        bytes[table + 4..table + 6].copy_from_slice(&16u16.to_le_bytes());
        bytes[table + 6..table + 8].copy_from_slice(&next.to_le_bytes());
        bytes[table + 8..table + 10].copy_from_slice(&frame.to_le_bytes());
        bytes[table + 10..table + 12].copy_from_slice(&60u16.to_le_bytes());
    }
    bytes[46..106].fill(0x33);
    bytes[110..170].fill(0x44);
    let block = ncm::decode(&bytes).unwrap();
    assert_eq!(block.count, 2);
    assert_eq!(block.datagrams[1].offset, 110);
    bytes[34..36].copy_from_slice(&12u16.to_le_bytes());
    assert!(ncm::decode(&bytes).is_err());
}

#[test]
fn batch_encoder_preserves_order_bounds_padding_and_full_block_retries() {
    for capacity in [2048, ncm::NTB_SIZE] {
        for sizes in [vec![14; 16], vec![1514; 16], vec![61, 62, 63, 64, 1514, 15]] {
            let mut output = vec![0xa5; capacity];
            let mut encoder = ncm::Encoder::new();
            let mut expected = Vec::new();
            for (index, size) in sizes.into_iter().enumerate() {
                let frame = vec![index as u8; size];
                let before = output.clone();
                if encoder.push(&frame, &mut output).is_err() {
                    assert_eq!(output, before); // Backpressure cannot overwrite an accepted frame.
                    break;
                }
                expected.push(frame);
            }
            let length = encoder.finish(u16::MAX, &mut output).unwrap();
            let block = ncm::decode(&output[..length]).unwrap();
            assert_eq!(block.count, expected.len());
            assert_eq!(&output[6..8], &[255, 255]);
            for (datagram, frame) in block.datagrams[..block.count].iter().zip(expected) {
                assert_eq!(
                    &output[datagram.offset..datagram.offset + datagram.length],
                    frame
                );
                assert_eq!((datagram.offset + 14) % 4, 0);
            }
            assert!(output[length..].iter().all(|&byte| byte == 0xa5));
            // Every gap actually transmitted must be initialized, even if the
            // DMA slot previously held unrelated traffic.
            let ndp = u16::from_le_bytes(output[10..12].try_into().unwrap()) as usize;
            for (index, &byte) in output.iter().enumerate().take(ndp).skip(12) {
                if !block.datagrams[..block.count]
                    .iter()
                    .any(|d| (d.offset..d.offset + d.length).contains(&index))
                {
                    assert_eq!(byte, 0, "uninitialized NTB padding at {index}");
                }
            }
        }
    }
    let mut output = [0xa5; ncm::NTB_SIZE];
    assert!(ncm::Encoder::new().finish(0, &mut output).is_err());
    assert!(output.iter().all(|&b| b == 0xa5));
}
