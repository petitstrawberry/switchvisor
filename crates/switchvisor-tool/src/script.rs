//! Reproducible legacy U-Boot script packaging; no external mkimage is needed.
use crate::{digest, read};
use serde_json::{Value, json};
use std::{ffi::OsString, fs, io::Write as _, path::Path};
use switchvisor::payload::{MAX_PACKAGE_SIZE, crc32};

pub fn pack(args: &[OsString]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(crate::USAGE.into());
    }
    let input = Path::new(&args[0]);
    if fs::metadata(input).map_err(|e| e.to_string())?.len() > MAX_PACKAGE_SIZE - 72 {
        return Err("script exceeds size limit".into());
    }
    let source = read(input)?;
    if source.is_empty() || source.contains(&0) || std::str::from_utf8(&source).is_err() {
        return Err("script must be nonempty UTF-8 without NUL".into());
    }
    let mut body = Vec::with_capacity(source.len() + 8);
    body.extend_from_slice(&(source.len() as u32).to_be_bytes());
    body.extend_from_slice(&[0; 4]);
    body.extend_from_slice(&source);
    let mut header = [0; 64];
    for (offset, value) in [(0, 0x27051956), (12, body.len() as u32), (24, crc32(&body))] {
        header[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
    // Match the native boot.scr convention; this contains shell text, not instructions.
    header[28..32].copy_from_slice(&[5, 2, 6, 0]); // Linux, ARM, script, uncompressed.
    let name = b"Switchvisor boot script";
    header[32..32 + name.len()].copy_from_slice(name);
    let checksum = crc32(&header);
    header[4..8].copy_from_slice(&checksum.to_be_bytes());
    let mut image = header.to_vec();
    image.extend_from_slice(&body);
    let output = Path::new(&args[1]);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| format!("{}: {e}", output.display()))?
        .write_all(&image)
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"kind":"uboot-script","input":input.display().to_string(),"input_sha256":digest(&source),
        "output":output.display().to_string(),"sha256":digest(&image),"file_size_bytes":image.len()}),
    )
}
