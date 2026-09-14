//! Bounded, read-only FDT traversal for boot-time validation, without allocation.
use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdtError {
    Truncated,
    Header,
    BlockOverlap,
    ReservationTable,
    Structure,
    String,
    PathCapacity,
    DuplicateProperty,
    OutputCapacity,
    MemoryCells,
    MemoryRanges,
    ReservationConflict,
}

impl fmt::Display for FdtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT {self:?}")
    }
}

fn word(bytes: &[u8], offset: usize) -> Result<usize, FdtError> {
    let end = offset.checked_add(4).ok_or(FdtError::Truncated)?;
    let data = bytes.get(offset..end).ok_or(FdtError::Truncated)?;
    Ok(u32::from_be_bytes(data.try_into().map_err(|_| FdtError::Truncated)?) as usize)
}

fn block(bytes: &[u8], start: usize, size: usize) -> Result<&[u8], FdtError> {
    bytes
        .get(start..start.checked_add(size).ok_or(FdtError::Truncated)?)
        .ok_or(FdtError::Truncated)
}

fn align4(value: usize) -> Result<usize, FdtError> {
    Ok(value.checked_add(3).ok_or(FdtError::Truncated)? & !3)
}

#[derive(Clone, Copy, Debug)]
pub struct Property<'a> {
    pub data_offset: usize,
    pub data: &'a [u8],
}

pub struct Fdt<'a> {
    bytes: &'a [u8],
    structure_offset: usize,
    structure: &'a [u8],
    strings: &'a [u8],
}

impl<'a> Fdt<'a> {
    pub fn parse(input: &'a [u8]) -> Result<Self, FdtError> {
        if input.len() < 40 || word(input, 0)? != 0xd00d_feed {
            return Err(FdtError::Header);
        }
        let total = word(input, 4)?;
        if total < 40 {
            return Err(FdtError::Header);
        }
        let bytes = input.get(..total).ok_or(FdtError::Truncated)?;
        let structure_offset = word(bytes, 8)?;
        let strings_offset = word(bytes, 12)?;
        let reservations_offset = word(bytes, 16)?;
        if word(bytes, 20)? != 17
            || word(bytes, 24)? > 17
            || structure_offset % 4 != 0
            || reservations_offset % 8 != 0
        {
            return Err(FdtError::Header);
        }
        let strings_size = word(bytes, 32)?;
        let structure_size = word(bytes, 36)?;
        let structure = block(bytes, structure_offset, structure_size)?;
        let strings = block(bytes, strings_offset, strings_size)?;
        let mut reservation_end = reservations_offset;
        loop {
            let entry =
                block(bytes, reservation_end, 16).map_err(|_| FdtError::ReservationTable)?;
            reservation_end += 16;
            if entry.iter().all(|b| *b == 0) {
                break;
            }
        }
        let ranges = [
            (0, 40),
            (structure_offset, structure_offset + structure_size),
            (strings_offset, strings_offset + strings_size),
            (reservations_offset, reservation_end),
        ];
        for (i, &(start, end)) in ranges.iter().enumerate() {
            if start == end {
                continue;
            }
            for &(other_start, other_end) in &ranges[..i] {
                if other_start != other_end && start < other_end && other_start < end {
                    return Err(FdtError::BlockOverlap);
                }
            }
        }
        let fdt = Self {
            bytes,
            structure_offset,
            structure,
            strings,
        };
        fdt.walk(|_, _, _| {})?;
        Ok(fdt)
    }

    pub fn total_size(&self) -> usize {
        self.bytes.len()
    }

    pub fn find_property(&self, path: &str, name: &str) -> Result<Option<Property<'a>>, FdtError> {
        let mut found = None;
        let mut duplicate = false;
        self.walk(|current_path, current_name, property| {
            if current_path == path && current_name == name {
                duplicate |= found.is_some();
                found = Some(property);
            }
        })?;
        if duplicate {
            return Err(FdtError::DuplicateProperty);
        }
        Ok(found)
    }

    pub(crate) fn walk(
        &self,
        mut visit: impl FnMut(&str, &str, Property<'a>),
    ) -> Result<(), FdtError> {
        let mut path = [0u8; 512];
        let mut saved_lengths = [0usize; 32];
        let mut path_len = 0;
        let mut depth = 0;
        let mut root_seen = false;
        let mut pos = 0;
        while pos < self.structure.len() {
            let token = word(self.structure, pos)?;
            pos += 4;
            match token {
                1 => {
                    let tail = self.structure.get(pos..).ok_or(FdtError::Truncated)?;
                    let length = tail.iter().position(|b| *b == 0).ok_or(FdtError::String)?;
                    let name =
                        core::str::from_utf8(&tail[..length]).map_err(|_| FdtError::String)?;
                    if depth == 0 {
                        if root_seen || !name.is_empty() {
                            return Err(FdtError::Structure);
                        }
                        root_seen = true;
                    } else if name.is_empty() || name.contains('/') {
                        return Err(FdtError::Structure);
                    }
                    if depth == saved_lengths.len() {
                        return Err(FdtError::PathCapacity);
                    }
                    saved_lengths[depth] = path_len;
                    depth += 1;
                    if !name.is_empty() {
                        if path_len + length + 1 > path.len() {
                            return Err(FdtError::PathCapacity);
                        }
                        path[path_len] = b'/';
                        path[path_len + 1..path_len + 1 + length].copy_from_slice(name.as_bytes());
                        path_len += length + 1;
                    }
                    pos = align4(pos + length + 1)?;
                }
                2 => {
                    if depth == 0 {
                        return Err(FdtError::Structure);
                    }
                    depth -= 1;
                    path_len = saved_lengths[depth];
                }
                3 => {
                    if depth == 0 {
                        return Err(FdtError::Structure);
                    }
                    let length = word(self.structure, pos)?;
                    let name_offset = word(self.structure, pos + 4)?;
                    pos += 8;
                    let data = block(self.structure, pos, length)?;
                    let tail = self.strings.get(name_offset..).ok_or(FdtError::String)?;
                    let name_len = tail.iter().position(|b| *b == 0).ok_or(FdtError::String)?;
                    let name =
                        core::str::from_utf8(&tail[..name_len]).map_err(|_| FdtError::String)?;
                    if name.is_empty() {
                        return Err(FdtError::String);
                    }
                    let current_path = if path_len == 0 {
                        "/"
                    } else {
                        core::str::from_utf8(&path[..path_len]).map_err(|_| FdtError::String)?
                    };
                    visit(
                        current_path,
                        name,
                        Property {
                            data_offset: self.structure_offset + pos,
                            data,
                        },
                    );
                    pos = align4(pos + length)?;
                }
                4 => (),
                9 => {
                    if !root_seen || depth != 0 {
                        return Err(FdtError::Structure);
                    }
                    if self.structure[pos..].iter().any(|b| *b != 0) {
                        return Err(FdtError::Structure);
                    }
                    return Ok(());
                }
                _ => return Err(FdtError::Structure),
            }
        }
        Err(FdtError::Structure)
    }
}
