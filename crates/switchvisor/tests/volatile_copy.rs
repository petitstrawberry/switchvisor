#[path = "../src/arch/aarch64/copy.rs"]
mod copy;

#[test]
fn volatile_bulk_copy_handles_every_alignment_tail_and_does_not_touch_neighbors() {
    let source: Vec<u8> = (0..1600).map(|n| (n * 37) as u8).collect();
    for guest_offset in 0..16 {
        for local_offset in 0..16 {
            for length in (0..33).chain([63, 64, 65, 1514, 1526]) {
                let mut output = [0xa5; 1600];
                unsafe {
                    copy::read(
                        source.as_ptr().add(guest_offset),
                        &mut output[local_offset..local_offset + length],
                    );
                }
                assert_eq!(
                    &output[local_offset..local_offset + length],
                    &source[guest_offset..guest_offset + length]
                );
                assert!(
                    output[..local_offset]
                        .iter()
                        .chain(&output[local_offset + length..])
                        .all(|&byte| byte == 0xa5)
                );
                output.fill(0xa5);
                unsafe {
                    copy::write(
                        output.as_mut_ptr().add(guest_offset),
                        &source[local_offset..local_offset + length],
                    );
                }
                assert_eq!(
                    &output[guest_offset..guest_offset + length],
                    &source[local_offset..local_offset + length]
                );
                assert!(
                    output[..guest_offset]
                        .iter()
                        .chain(&output[guest_offset + length..])
                        .all(|&byte| byte == 0xa5)
                );
            }
        }
    }
}
