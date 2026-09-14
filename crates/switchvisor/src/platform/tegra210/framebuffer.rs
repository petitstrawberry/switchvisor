//! Writable Hekate scanout adapter; no display-controller reprogramming.
use switchvisor::framebuffer::{Console, FramebufferError, FramebufferLayout, PixelSink};

pub struct Scanout {
    base: u64,
}

impl PixelSink for Scanout {
    fn write_pixel(&mut self, byte_offset: u64, bgra: u32) {
        // The console supplies only validated offsets into active Hekate scanout RAM.
        unsafe {
            core::ptr::write_volatile((self.base + byte_offset) as *mut u32, bgra);
        }
    }
}

pub type Screen = Console<Scanout>;

pub fn console() -> Result<Screen, FramebufferError> {
    let layout = FramebufferLayout::hekate_erista()?;
    Ok(Console::new(
        layout,
        Scanout {
            base: layout.reservation().start(),
        },
    ))
}
