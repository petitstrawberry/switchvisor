use std::{cell::RefCell, fmt::Write, rc::Rc};
use switchvisor_core::framebuffer::{Console, FramebufferLayout, PixelSink, Rotation};

struct Sink(Rc<RefCell<Vec<u32>>>);
impl PixelSink for Sink {
    fn write_pixel(&mut self, offset: u64, bgra: u32) {
        assert_eq!(offset % 4, 0);
        self.0.borrow_mut()[offset as usize / 4] = bgra;
    }
}

#[test]
fn inherited_rotation_and_bgra_addresses_match_native_scanout() {
    let layout = FramebufferLayout::hekate_erista().unwrap();
    assert_eq!(
        (layout.logical_width(), layout.logical_height()),
        (1280, 720)
    );
    assert_eq!(layout.pixel_offset(0, 0), Some(1279 * 2880));
    assert_eq!(layout.pixel_offset(1279, 719), Some(719 * 4));
    assert_eq!(layout.pixel_offset(1280, 0), None);
    assert_eq!(layout.pixel_offset(0, 720), None);
    assert_eq!(layout.reservation().start(), 0xf5a0_0000);
    assert_eq!(0xffff_0000u32.to_le_bytes(), [0, 0, 255, 255]);
}

#[test]
fn text_has_visible_glyphs_and_wraps_without_writing_outside_the_surface() {
    let layout = FramebufferLayout::new(0x1000, 64 * 64 * 4, 64, 64, 256, Rotation::Three).unwrap();
    let pixels = Rc::new(RefCell::new(vec![0; 64 * 64]));
    let mut console = Console::new(layout, Sink(pixels.clone()));
    console.clear();
    writeln!(console, "EL2").unwrap();
    assert!(pixels.borrow().contains(&0xffee_f2f6));
    write!(console, "{}\t\rEND", "0123456789ABCDEF\n".repeat(40)).unwrap();
    assert!(
        pixels
            .borrow()
            .iter()
            .all(|p| *p == 0xff12_1827 || *p == 0xffee_f2f6)
    );
}

#[test]
fn invalid_geometry_and_overflow_are_rejected() {
    assert!(FramebufferLayout::new(u64::MAX - 4, 16384, 64, 64, 256, Rotation::None).is_err());
    assert!(FramebufferLayout::new(0x1000, 16384, 0, 64, 256, Rotation::None).is_err());
    assert!(FramebufferLayout::new(0x1000, 16384, 64, 64, 252, Rotation::None).is_err());
    assert!(FramebufferLayout::new(0x1000, 16383, 64, 64, 256, Rotation::None).is_err());
    assert!(FramebufferLayout::new(0x1001, 16384, 64, 64, 256, Rotation::None).is_err());
}
