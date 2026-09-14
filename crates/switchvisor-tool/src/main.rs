mod payload;
mod profile;

use std::{collections::BTreeMap, env, fs, io::Write as _, path::Path, process::ExitCode};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use switchvisor_core::{
    fdt::Fdt,
    image::{ScarletImage, UbootImage},
};

const USAGE: &str = "Usage: switchvisor-tool <command> <path>\n\nCommands:\n  inspect-bootstack <directory>    Verify the pinned BL31/BL33/nx-plat.dtimg\n  inspect-image <Image>            Inspect a raw Scarlet Linux Image\n  validate-profile <profile.json>  Validate complete, declared physical geometry\n  pack-diagnostic <raw> <bootstack-directory> <output.bin>\n                                  Append the pinned Hekate probe FDT\n  pack-payload <bootstrap.raw> <bootstack-directory> <payload.raw> <runtime-size> <output.bin>\n               [--entry-offset <number>] [--x0 <number> ... --x7 <number>]\n                                  Inject an external raw EL1 payload\n\nCommands print JSON. Packers create new files; no command installs or boots an image.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pins {
    source: String,
    uboot_source_revision: String,
    hekate_revision: String,
    files: BTreeMap<String, String>,
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn inspect_bootstack(directory: &Path) -> Result<Value, String> {
    let pins: Pins = serde_json::from_str(include_str!("../../../config/bootstack.json"))
        .map_err(|e| e.to_string())?;
    let mut files = BTreeMap::new();
    let mut bl33 = None;
    for (name, expected) in &pins.files {
        let bytes = read(&directory.join(name))?;
        let actual = digest(&bytes);
        if actual != *expected {
            return Err(format!(
                "{name}: SHA256 mismatch; expected {expected}, got {actual}"
            ));
        }
        files.insert(name, json!({"size_bytes": bytes.len(), "sha256": actual}));
        if name == "bl33.bin" {
            bl33 = Some(bytes);
        }
    }
    let bytes = bl33.ok_or("pin file does not contain bl33.bin")?;
    let image = UbootImage::parse(&bytes).map_err(|e| format!("bl33.bin: {e}"))?;
    image
        .validate_hekate_patches(&bytes)
        .map_err(|e| e.to_string())?;
    let mut cases = Vec::new();
    for port in 0..=3 {
        let mut patched = bytes.clone();
        image
            .apply_hekate_uart_patch(&mut patched, port)
            .map_err(|e| e.to_string())?;
        let offset = image.end_offset as usize;
        let fdt = Fdt::parse(&patched[offset..]).map_err(|e| e.to_string())?;
        if port != 0 {
            let address = ["70006000", "70006040", "70006200"][usize::from(port - 1)];
            let node = format!("/serial@{address}");
            for name in ["stdout-path", "stderr-path"] {
                let value = fdt
                    .find_property("/chosen", name)
                    .map_err(|e| e.to_string())?
                    .ok_or("missing console property")?;
                if value.data.split(|b| *b == 0).next() != Some(node.as_bytes()) {
                    return Err("Hekate console patch verification failed".into());
                }
            }
            let value = fdt
                .find_property(&node, "status")
                .map_err(|e| e.to_string())?
                .ok_or("missing UART status")?;
            if value.data.split(|b| *b == 0).next() != Some(b"okay".as_slice()) {
                return Err("Hekate UART status verification failed".into());
            }
        }
        let status = offset + 0x1c94 + usize::from(port.saturating_sub(1)) * 0x12c;
        let allowed = [
            (status, status + 5),
            (offset + 0x3f34, offset + 0x3f34 + 9),
            (offset + 0x3f54, offset + 0x3f54 + 9),
        ];
        for (i, (before, after)) in bytes.iter().zip(&patched).enumerate() {
            if before != after
                && (port == 0 || !allowed.iter().any(|&(start, end)| start <= i && i < end))
            {
                return Err("Hekate patch modified unrelated bytes".into());
            }
        }
        cases.push(json!({"uart_port": port, "passed": true}));
    }
    Ok(json!({
        "hardware_validated": false,
        "source": pins.source,
        "uboot_source_revision": pins.uboot_source_revision,
        "hekate_revision": pins.hekate_revision,
        "files": files,
        "uboot": {
            "linked_base": format!("{:#x}", image.linked_base),
            "end_offset": format!("{:#x}", image.end_offset),
            "bss_start_offset": format!("{:#x}", image.bss_start_offset),
            "bss_end_offset": format!("{:#x}", image.bss_end_offset),
            "control_fdt_size_bytes": image.control_fdt_size,
            "runtime_base": format!("{:#x}", image.runtime.start()),
            "runtime_size_bytes": image.runtime.size()
        },
        "hekate_uart_patch_cases": cases
    }))
}

fn inspect_image(path: &Path) -> Result<Value, String> {
    let bytes = read(path)?;
    let image = ScarletImage::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(json!({
        "hardware_validated": false,
        "sha256": digest(&bytes), "file_size_bytes": bytes.len(),
        "text_offset": format!("{:#x}", image.text_offset),
        "runtime_base": format!("{:#x}", image.runtime.start()),
        "runtime_end": format!("{:#x}", image.runtime.end()),
        "runtime_size_bytes": image.image_size
    }))
}

fn pack_diagnostic(raw_path: &Path, directory: &Path, output: &Path) -> Result<Value, String> {
    let bootstack = inspect_bootstack(directory)?;
    let raw = read(raw_path)?;
    if raw.get(40..48) != Some(b"SVBOOT01".as_slice()) {
        return Err("raw image is not a Switchvisor EL2 diagnostic".into());
    }
    let field = |offset| -> Result<u64, String> {
        let bytes = raw
            .get(offset..offset + 8)
            .ok_or("truncated diagnostic header")?;
        Ok(u64::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| "truncated diagnostic header")?,
        ))
    };
    if field(8)? != switchvisor_core::BL33_LOAD_BASE
        || field(16)? != raw.len() as u64
        || field(24)? != raw.len() as u64
        || field(32)? < raw.len() as u64
        || field(32)? > 1024 * 1024
    {
        return Err("diagnostic header/load address/runtime extent mismatch".into());
    }
    let slot = raw
        .get(
            switchvisor_core::payload::CONFIG_OFFSET
                ..switchvisor_core::payload::CONFIG_OFFSET + switchvisor_core::payload::CONFIG_SIZE,
        )
        .ok_or("diagnostic has no payload descriptor slot")?;
    if slot.iter().any(|byte| *byte != 0) {
        return Err("diagnostic payload descriptor must be empty".into());
    }
    let original = read(&directory.join("bl33.bin"))?;
    if Some(digest(&original).as_str()) != bootstack["files"]["bl33.bin"]["sha256"].as_str() {
        return Err("bl33.bin changed after bootstack verification".into());
    }
    let original_image = UbootImage::parse(&original).map_err(|e| e.to_string())?;
    let start = original_image.end_offset as usize;
    let probe = &original[start..start + original_image.control_fdt_size];
    let mut bytes = raw.clone();
    bytes.extend_from_slice(probe);
    let image = UbootImage::parse(&bytes).map_err(|e| e.to_string())?;
    image
        .validate_hekate_patches(&bytes)
        .map_err(|e| e.to_string())?;
    if image.runtime.size() > 1024 * 1024 {
        return Err("diagnostic exceeds transient BL33 budget".into());
    }
    for port in 0..=3 {
        let mut patched = bytes.clone();
        image
            .apply_hekate_uart_patch(&mut patched, port)
            .map_err(|e| e.to_string())?;
        Fdt::parse(&patched[raw.len()..]).map_err(|e| e.to_string())?;
    }
    // Refuse to overwrite any existing file, including Native boot assets and input binaries.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| format!("{}: {e}", output.display()))?;
    file.write_all(&bytes)
        .map_err(|e| format!("{}: {e}", output.display()))?;
    Ok(json!({
        "kind": "el2-entry-diagnostic", "hardware_validated": false, "guest_started": false,
        "output": output.display().to_string(), "sha256": digest(&bytes), "file_size_bytes": bytes.len(),
        "raw_sha256": digest(&raw), "probe_fdt_sha256": digest(probe), "probe_fdt_offset": raw.len(),
        "load_base": format!("{:#x}", image.runtime.start()), "runtime_size_bytes": image.runtime.size(),
        "profile": null, "resident_vmm": false,
        "bootstack": bootstack
    }))
}

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "help" || args[0] == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|command| command == "pack-payload")
    {
        let result = payload::pack(&args[1..])?;
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if args.len() == 4 && args[0] == "pack-diagnostic" {
        let result = pack_diagnostic(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
        )?;
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if args.len() != 2 {
        return Err(USAGE.into());
    }
    let path = Path::new(&args[1]);
    let result = match args[0].to_str() {
        Some("inspect-bootstack") => inspect_bootstack(path)?,
        Some("inspect-image") => inspect_image(path)?,
        Some("validate-profile") => profile::validate(&read(path)?)?,
        _ => return Err(USAGE.into()),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
