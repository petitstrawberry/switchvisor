//! Copy a DTB while subtracting resident EL2 RAM and preserving firmware reservations.
//! The output uses a canonical block order; no input offsets or native DTBs are changed.

use crate::{
    fdt::{Fdt, FdtError},
    memory::AddressRange,
};

pub const RESIDENT_PROPERTY: &str = "switchvisor,resident-region";
const MAX_MEMORY_NODES: usize = 16;

fn word(bytes: &[u8], offset: usize) -> Result<usize, FdtError> {
    let data = bytes.get(offset..offset + 4).ok_or(FdtError::Truncated)?;
    Ok(u32::from_be_bytes(data.try_into().map_err(|_| FdtError::Truncated)?) as usize)
}

fn quad(bytes: &[u8], offset: usize) -> Result<u64, FdtError> {
    let data = bytes.get(offset..offset + 8).ok_or(FdtError::Truncated)?;
    Ok(u64::from_be_bytes(
        data.try_into().map_err(|_| FdtError::Truncated)?,
    ))
}

fn cells(fdt: &Fdt<'_>, name: &str, default: usize) -> Result<usize, FdtError> {
    let value = match fdt.find_property("/", name)? {
        Some(property) if property.data.len() == 4 => word(property.data, 0)?,
        Some(_) => return Err(FdtError::MemoryCells),
        None => default,
    };
    if value != 1 && value != 2 {
        return Err(FdtError::MemoryCells);
    }
    Ok(value)
}

fn value(data: &[u8], offset: usize, cells: usize) -> Result<u64, FdtError> {
    if cells == 1 {
        Ok(word(data, offset)? as u64)
    } else {
        quad(data, offset)
    }
}

fn ranges(
    data: &[u8],
    address_cells: usize,
    size_cells: usize,
    region: AddressRange,
    mut emit: impl FnMut(u64, u64) -> Result<(), FdtError>,
) -> Result<(), FdtError> {
    let stride = (address_cells + size_cells) * 4;
    if data.len() % stride != 0 {
        return Err(FdtError::MemoryRanges);
    }
    for bank in data.chunks_exact(stride) {
        let start = value(bank, 0, address_cells)?;
        let size = value(bank, address_cells * 4, size_cells)?;
        let end = start.checked_add(size).ok_or(FdtError::MemoryRanges)?;
        if size == 0 {
            continue;
        }
        if start >= region.end() || end <= region.start() {
            emit(start, size)?;
        } else {
            if start < region.start() {
                emit(start, region.start() - start)?;
            }
            if end > region.end() {
                emit(region.end(), end - region.end())?;
            }
        }
    }
    Ok(())
}

struct Writer<'a> {
    bytes: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn bytes(&mut self, data: &[u8]) -> Result<(), FdtError> {
        let end = self
            .pos
            .checked_add(data.len())
            .ok_or(FdtError::OutputCapacity)?;
        self.bytes
            .get_mut(self.pos..end)
            .ok_or(FdtError::OutputCapacity)?
            .copy_from_slice(data);
        self.pos = end;
        Ok(())
    }
    fn word(&mut self, value: usize) -> Result<(), FdtError> {
        self.bytes(
            &u32::try_from(value)
                .map_err(|_| FdtError::OutputCapacity)?
                .to_be_bytes(),
        )
    }
    fn number(&mut self, value: u64, cells: usize) -> Result<(), FdtError> {
        if cells == 1 {
            self.bytes(
                &u32::try_from(value)
                    .map_err(|_| FdtError::MemoryRanges)?
                    .to_be_bytes(),
            )
        } else {
            self.bytes(&value.to_be_bytes())
        }
    }
    fn align4(&mut self) -> Result<(), FdtError> {
        while self.pos % 4 != 0 {
            self.bytes(&[0])?;
        }
        Ok(())
    }
    fn policy(&mut self, name_offset: usize, region: AddressRange) -> Result<(), FdtError> {
        self.word(3)?;
        self.word(16)?;
        self.word(name_offset)?;
        self.number(region.start(), 2)?;
        self.number(region.size(), 2)
    }
}

/// Subtracts the region from all root memory nodes, adds /memreserve/, and publishes
/// the same region at /chosen/switchvisor,resident-region for boot-policy consumers.
/// The caller supplies separate storage. Errors never mutate the input DTB.
pub fn exclude(input: &[u8], output: &mut [u8], region: AddressRange) -> Result<usize, FdtError> {
    let fdt = Fdt::parse(input)?;
    let input = &input[..fdt.total_size()];
    let address_cells = cells(&fdt, "#address-cells", 2)?;
    let size_cells = cells(&fdt, "#size-cells", 1)?;
    let mut memory = [(0usize, 0usize); MAX_MEMORY_NODES];
    let mut count = 0;
    let mut memory_error = Ok(());
    fdt.walk(|path, name, property| {
        if name != "device_type" || property.data != b"memory\0" || path[1..].contains('/') {
            return;
        }
        let result = (|| {
            let reg = fdt
                .find_property(path, "reg")?
                .ok_or(FdtError::MemoryRanges)?;
            // Detect duplicate device_type/reg properties before writing output.
            fdt.find_property(path, "device_type")?;
            if count == memory.len() {
                return Err(FdtError::MemoryRanges);
            }
            ranges(reg.data, address_cells, size_cells, region, |_, _| Ok(()))?;
            memory[count] = (reg.data_offset, reg.data.len());
            count += 1;
            Ok(())
        })();
        if result.is_err() {
            memory_error = result;
        }
    })?;
    memory_error?;

    let reserve_start = word(input, 16)?;
    let mut reserve_end = reserve_start;
    let mut already_reserved = false;
    loop {
        let start = quad(input, reserve_end)?;
        let size = quad(input, reserve_end + 8)?;
        if start == 0 && size == 0 {
            break;
        }
        if size != 0 {
            let reserved = AddressRange::new(start, size).map_err(|_| FdtError::MemoryRanges)?;
            if reserved.overlaps(region) {
                if reserved != region {
                    return Err(FdtError::ReservationConflict);
                }
                already_reserved = true;
            }
        }
        reserve_end += 16;
    }
    let struct_start = word(input, 8)?;
    let structure = &input[struct_start..struct_start + word(input, 36)?];
    let strings_start = word(input, 12)?;
    let strings = &input[strings_start..strings_start + word(input, 32)?];
    let mut name_offset = None;
    let mut pos = 0;
    while pos < strings.len() {
        let length = strings[pos..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(FdtError::String)?;
        if &strings[pos..pos + length] == RESIDENT_PROPERTY.as_bytes() {
            name_offset = Some(pos);
        }
        pos += length + 1;
    }
    let append_name = name_offset.is_none();
    let name_offset = name_offset.unwrap_or(strings.len());
    let mut writer = Writer {
        bytes: output,
        pos: 0,
    };
    writer.bytes(&input[..40])?;
    writer.bytes(&[0; 8])?;
    let output_reserve = writer.pos;
    writer.bytes(&input[reserve_start..reserve_end])?;
    if !already_reserved {
        writer.number(region.start(), 2)?;
        writer.number(region.size(), 2)?;
    }
    writer.bytes(&[0; 16])?;
    let output_structure = writer.pos;
    let mut depth = 0;
    let mut chosen = false;
    let mut chosen_seen = false;
    let mut cursor = 0;
    while cursor < structure.len() {
        let start = cursor;
        let token = word(structure, cursor)?;
        cursor += 4;
        match token {
            1 => {
                let length = structure[cursor..]
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or(FdtError::String)?;
                let is_chosen = depth == 1 && &structure[cursor..cursor + length] == b"chosen";
                cursor = (cursor + length + 4) & !3;
                writer.bytes(&structure[start..cursor])?;
                if is_chosen {
                    if chosen_seen {
                        return Err(FdtError::Structure);
                    }
                    chosen = true;
                    chosen_seen = true;
                    writer.policy(name_offset, region)?;
                }
                depth += 1;
            }
            2 => {
                if depth == 1 && !chosen_seen {
                    writer.word(1)?;
                    writer.bytes(b"chosen\0")?;
                    writer.align4()?;
                    writer.policy(name_offset, region)?;
                    writer.word(2)?;
                }
                if depth == 2 && chosen {
                    chosen = false;
                }
                depth -= 1;
                writer.word(2)?;
            }
            3 => {
                let length = word(structure, cursor)?;
                let prop_name = word(structure, cursor + 4)?;
                cursor += 8;
                let data_start = cursor;
                let data = &structure[cursor..cursor + length];
                cursor = (cursor + length + 3) & !3;
                let tail = &strings[prop_name..];
                let name_len = tail
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or(FdtError::String)?;
                if chosen && depth == 2 && &tail[..name_len] == RESIDENT_PROPERTY.as_bytes() {
                    continue;
                }
                if memory[..count]
                    .iter()
                    .any(|&(offset, _)| offset == struct_start + data_start)
                {
                    let mut banks = 0;
                    ranges(data, address_cells, size_cells, region, |_, _| {
                        banks += 1;
                        Ok(())
                    })?;
                    writer.word(3)?;
                    writer.word(banks * (address_cells + size_cells) * 4)?;
                    writer.word(prop_name)?;
                    ranges(data, address_cells, size_cells, region, |start, size| {
                        writer.number(start, address_cells)?;
                        writer.number(size, size_cells)
                    })?;
                } else {
                    writer.bytes(&structure[start..cursor])?;
                }
            }
            4 => writer.word(token)?,
            9 => {
                writer.word(9)?;
                break;
            }
            _ => return Err(FdtError::Structure),
        }
    }
    let output_strings = writer.pos;
    writer.bytes(strings)?;
    if append_name {
        writer.bytes(RESIDENT_PROPERTY.as_bytes())?;
        writer.bytes(&[0])?;
    }
    let size = writer.pos;
    for (offset, value) in [
        (4, size),
        (8, output_structure),
        (12, output_strings),
        (16, output_reserve),
        (32, size - output_strings),
        (36, output_strings - output_structure),
    ] {
        writer.bytes[offset..offset + 4].copy_from_slice(
            &u32::try_from(value)
                .map_err(|_| FdtError::OutputCapacity)?
                .to_be_bytes(),
        );
    }
    Fdt::parse(&writer.bytes[..size])?;
    Ok(size)
}
