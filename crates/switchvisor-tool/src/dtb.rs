use crate::{digest, read};
use serde_json::{Value, json};
use std::{ffi::OsString, fs, io::Write as _, path::Path};
use switchvisor_core::{
    fdt_memory,
    memory::AddressRange,
    payload::{MAX_PACKAGE_SIZE, RESIDENT_BASE, RESIDENT_SIZE},
};

pub fn prepare(args: &[OsString]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(crate::USAGE.into());
    }
    let input = Path::new(&args[0]);
    if fs::metadata(input).map_err(|e| e.to_string())?.len() > MAX_PACKAGE_SIZE {
        return Err("input exceeds 64 MiB".into());
    }
    let data = read(input)?;
    let region = AddressRange::new(RESIDENT_BASE, RESIDENT_SIZE).map_err(|e| e.to_string())?;
    let mut dtb = vec![0; data.len() + 4096];
    let size = fdt_memory::exclude(&data, &mut dtb, region).map_err(|e| e.to_string())?;
    dtb.truncate(size);
    // All parsing and policy checks precede output creation. Never replace inputs/native files.
    let output = Path::new(&args[1]);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| format!("{}: {e}", output.display()))?
        .write_all(&dtb)
        .map_err(|e| e.to_string())?;
    Ok(json!({"kind":"guest-dtb",
        "input":input.display().to_string(),"input_sha256":digest(&data),
        "output":output.display().to_string(),"sha256":digest(&dtb),
        "resident_base":format!("{RESIDENT_BASE:#x}"),"resident_size_bytes":RESIDENT_SIZE,
        "file_size_bytes":dtb.len()}))
}
