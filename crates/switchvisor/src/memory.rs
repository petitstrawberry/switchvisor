use core::fmt;

/// A nonempty half-open physical range. Construction rejects address overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressRange {
    start: u64,
    end: u64,
}

impl AddressRange {
    pub fn new(start: u64, size: u64) -> Result<Self, RangeError> {
        if size == 0 {
            return Err(RangeError::Empty);
        }
        let end = start.checked_add(size).ok_or(RangeError::Overflow)?;
        Ok(Self { start, end })
    }

    pub const fn start(self) -> u64 {
        self.start
    }
    pub const fn end(self) -> u64 {
        self.end
    }
    pub const fn size(self) -> u64 {
        self.end - self.start
    }
    pub const fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    Empty,
    Overflow,
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty address range",
            Self::Overflow => "address range overflows u64",
        })
    }
}
