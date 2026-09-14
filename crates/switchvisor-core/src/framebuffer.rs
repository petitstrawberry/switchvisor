//! Integer-only BGRA boot console. The sink owns the volatile MMIO/memory boundary.
use core::fmt;

use crate::{IPA_LIMIT, memory::AddressRange};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    None,
    Three,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FramebufferError {
    Geometry,
    Address,
}

#[derive(Clone, Copy, Debug)]
pub struct FramebufferLayout {
    reservation: AddressRange,
    width: u32,
    height: u32,
    stride: u32,
    rotation: Rotation,
}

impl FramebufferLayout {
    pub fn new(
        base: u64,
        size: u64,
        width: u32,
        height: u32,
        stride: u32,
        rotation: Rotation,
    ) -> Result<Self, FramebufferError> {
        if !(64..=4096).contains(&width)
            || !(64..=4096).contains(&height)
            || stride % 4 != 0
            || u64::from(stride) < u64::from(width) * 4
            || u64::from(stride) * u64::from(height) > size
            || size > 16 * 1024 * 1024
        {
            return Err(FramebufferError::Geometry);
        }
        let reservation = AddressRange::new(base, size).map_err(|_| FramebufferError::Address)?;
        if base == 0 || base % 4 != 0 || reservation.end() > IPA_LIMIT {
            return Err(FramebufferError::Address);
        }
        Ok(Self {
            reservation,
            width,
            height,
            stride,
            rotation,
        })
    }

    /// The inspected Hekate Erista scanout; no display controller reprogramming.
    pub fn hekate_erista() -> Result<Self, FramebufferError> {
        Self::new(0xf5a0_0000, 0x40_0000, 720, 1280, 2880, Rotation::Three)
    }

    pub fn reservation(self) -> AddressRange {
        self.reservation
    }
    pub fn logical_width(self) -> u32 {
        match self.rotation {
            Rotation::None => self.width,
            Rotation::Three => self.height,
        }
    }
    pub fn logical_height(self) -> u32 {
        match self.rotation {
            Rotation::None => self.height,
            Rotation::Three => self.width,
        }
    }

    pub fn pixel_offset(self, x: u32, y: u32) -> Option<u64> {
        if x >= self.logical_width() || y >= self.logical_height() {
            return None;
        }
        let (physical_x, physical_y) = match self.rotation {
            Rotation::None => (x, y),
            Rotation::Three => (y, self.height - 1 - x),
        };
        Some(u64::from(physical_y) * u64::from(self.stride) + u64::from(physical_x) * 4)
    }
}

/// Offsets originate only from a validated layout; implementations must preserve BGRA pixels.
pub trait PixelSink {
    fn write_pixel(&mut self, byte_offset: u64, bgra: u32);
}

pub struct Console<S> {
    layout: FramebufferLayout,
    sink: S,
    column: u32,
    row: u32,
    foreground: u32,
    background: u32,
}

const MARGIN: u32 = 16;
const CELL_WIDTH: u32 = 12;
const CELL_HEIGHT: u32 = 16;

impl<S: PixelSink> Console<S> {
    pub fn new(layout: FramebufferLayout, sink: S) -> Self {
        Self {
            layout,
            sink,
            column: 0,
            row: 0,
            foreground: 0xffee_f2f6,
            background: 0xff12_1827,
        }
    }

    pub fn clear(&mut self) {
        for y in 0..self.layout.logical_height() {
            for x in 0..self.layout.logical_width() {
                self.pixel(x, y, self.background);
            }
        }
        self.column = 0;
        self.row = 0;
    }

    fn pixel(&mut self, x: u32, y: u32, color: u32) {
        if let Some(offset) = self.layout.pixel_offset(x, y) {
            self.sink.write_pixel(offset, color);
        }
    }

    fn newline(&mut self) {
        self.column = 0;
        self.row = (self.row + 1) % ((self.layout.logical_height() - MARGIN * 2) / CELL_HEIGHT);
        let y = MARGIN + self.row * CELL_HEIGHT;
        for dy in 0..CELL_HEIGHT {
            for x in MARGIN..self.layout.logical_width() - MARGIN {
                self.pixel(x, y + dy, self.background);
            }
        }
    }

    fn byte(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.newline();
                return;
            }
            b'\r' => {
                self.column = 0;
                return;
            }
            b'\t' => {
                for _ in 0..4 - self.column % 4 {
                    self.byte(b' ');
                }
                return;
            }
            _ => (),
        }
        if self.column == (self.layout.logical_width() - MARGIN * 2) / CELL_WIDTH {
            self.newline();
        }
        let glyph = glyph(byte.to_ascii_uppercase());
        let x = MARGIN + self.column * CELL_WIDTH;
        let y = MARGIN + self.row * CELL_HEIGHT;
        for dy in 0..CELL_HEIGHT {
            for dx in 0..CELL_WIDTH {
                let lit = dy < 14 && dx < 10 && glyph[(dy / 2) as usize] & (1 << (4 - dx / 2)) != 0;
                self.pixel(
                    x + dx,
                    y + dy,
                    if lit {
                        self.foreground
                    } else {
                        self.background
                    },
                );
            }
        }
        self.column += 1;
    }
}

impl<S: PixelSink> fmt::Write for Console<S> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            self.byte(byte);
        }
        Ok(())
    }
}

// Compact, locally defined 5x7 uppercase diagnostic font, scaled 2x with integer math.
fn glyph(byte: u8) -> [u8; 7] {
    match byte {
        b' ' => [0; 7],
        b'A' => [14, 17, 17, 31, 17, 17, 17],
        b'B' => [30, 17, 17, 30, 17, 17, 30],
        b'C' => [14, 17, 16, 16, 16, 17, 14],
        b'D' => [30, 17, 17, 17, 17, 17, 30],
        b'E' => [31, 16, 16, 30, 16, 16, 31],
        b'F' => [31, 16, 16, 30, 16, 16, 16],
        b'G' => [14, 17, 16, 23, 17, 17, 15],
        b'H' => [17, 17, 17, 31, 17, 17, 17],
        b'I' => [14, 4, 4, 4, 4, 4, 14],
        b'J' => [7, 2, 2, 2, 2, 18, 12],
        b'K' => [17, 18, 20, 24, 20, 18, 17],
        b'L' => [16, 16, 16, 16, 16, 16, 31],
        b'M' => [17, 27, 21, 21, 17, 17, 17],
        b'N' => [17, 25, 21, 19, 17, 17, 17],
        b'O' => [14, 17, 17, 17, 17, 17, 14],
        b'P' => [30, 17, 17, 30, 16, 16, 16],
        b'Q' => [14, 17, 17, 17, 21, 18, 13],
        b'R' => [30, 17, 17, 30, 20, 18, 17],
        b'S' => [15, 16, 16, 14, 1, 1, 30],
        b'T' => [31, 4, 4, 4, 4, 4, 4],
        b'U' => [17, 17, 17, 17, 17, 17, 14],
        b'V' => [17, 17, 17, 17, 17, 10, 4],
        b'W' => [17, 17, 17, 21, 21, 21, 10],
        b'X' => [17, 17, 10, 4, 10, 17, 17],
        b'Y' => [17, 17, 10, 4, 4, 4, 4],
        b'Z' => [31, 1, 2, 4, 8, 16, 31],
        b'0' => [14, 17, 19, 21, 25, 17, 14],
        b'1' => [4, 12, 4, 4, 4, 4, 14],
        b'2' => [14, 17, 1, 2, 4, 8, 31],
        b'3' => [30, 1, 1, 14, 1, 1, 30],
        b'4' => [2, 6, 10, 18, 31, 2, 2],
        b'5' => [31, 16, 16, 30, 1, 1, 30],
        b'6' => [14, 16, 16, 30, 17, 17, 14],
        b'7' => [31, 1, 2, 4, 8, 8, 8],
        b'8' => [14, 17, 17, 14, 17, 17, 14],
        b'9' => [14, 17, 17, 15, 1, 1, 14],
        b'=' => [0, 0, 31, 0, 31, 0, 0],
        b':' => [0, 4, 4, 0, 4, 4, 0],
        b'.' => [0, 0, 0, 0, 0, 4, 4],
        b',' => [0, 0, 0, 0, 0, 4, 8],
        b'-' => [0, 0, 0, 31, 0, 0, 0],
        b'_' => [0, 0, 0, 0, 0, 0, 31],
        b'/' => [1, 2, 2, 4, 8, 8, 16],
        b'[' => [14, 8, 8, 8, 8, 8, 14],
        b']' => [14, 2, 2, 2, 2, 2, 14],
        b'(' => [2, 4, 8, 8, 8, 4, 2],
        b')' => [8, 4, 2, 2, 2, 4, 8],
        b'+' => [0, 4, 4, 31, 4, 4, 0],
        b'!' => [4, 4, 4, 4, 4, 0, 4],
        b'<' => [1, 2, 4, 8, 4, 2, 1],
        b'>' => [16, 8, 4, 2, 4, 8, 16],
        _ => [14, 17, 1, 2, 4, 0, 4],
    }
}
