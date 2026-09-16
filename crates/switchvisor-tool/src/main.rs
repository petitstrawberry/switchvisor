mod dtb;
mod payload;
mod script;

use std::{collections::BTreeMap, env, fs, path::Path, process::ExitCode};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use switchvisor::{fdt::Fdt, image::UbootImage};

const USAGE: &str = "Usage: switchvisor-tool <command> [arguments]\n\nCommands:\n  inspect-bootstack <directory>    Verify the pinned BL31/BL33/nx-plat.dtimg\n  prepare-dtb <input.dtb> <output.dtb>\n                                  Exclude resident EL2 RAM from a guest DTB\n  pack-payload <bootstrap.raw> <bootstack-directory> <payload.raw> <runtime-size> <output.bin>\n               [--entry-offset <number>] [--x0 <number> ... --x7 <number>] [--usb-uart] [--usb-control] [--usb-gdb] [--no-fallback]\n                                  Inject an external raw EL1 payload\n  pack-script <boot.cmd> <boot.scr>  Package a legacy U-Boot boot script\n\nCommands print JSON. Packers create new files; no command installs or boots an image.";

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
    let pins: Pins = serde_json::from_str(include_str!("../config/bootstack.json"))
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

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "help" || args[0] == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    if args.first().is_some_and(|command| command == "prepare-dtb") {
        let result = dtb::prepare(&args[1..])?;
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if args.first().is_some_and(|command| command == "pack-script") {
        let result = script::pack(&args[1..])?;
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
        );
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
    if args.len() != 2 {
        return Err(USAGE.into());
    }
    let path = Path::new(&args[1]);
    let result = match args[0].to_str() {
        Some("inspect-bootstack") => inspect_bootstack(path)?,
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
