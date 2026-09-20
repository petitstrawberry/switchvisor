use std::{ffi::OsString, fs, io::Write as _, path::Path};

use serde_json::{Value, json};
use switchvisor::{
    image::UbootImage,
    payload::{
        CONFIG_OFFSET, CONFIG_SIZE, LOAD_BASE, MAX_PACKAGE_SIZE, Payload, RESIDENT_BASE,
        RESIDENT_SIZE, STACK_TOP, crc32,
    },
};

use crate::{digest, inspect_bootstack, read};

fn number(value: &std::ffi::OsStr) -> Result<u64, String> {
    let value = value.to_str().ok_or("number is not UTF-8")?;
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    }
    .map_err(|e| format!("invalid number {value:?}: {e}"))
}

fn field(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or("truncated bootstrap header")?
            .try_into()
            .map_err(|_| "truncated bootstrap header")?,
    ))
}

pub fn pack(args: &[OsString]) -> Result<Value, String> {
    if args.len() < 5 {
        return Err(crate::USAGE.into());
    }
    let mut entry_offset = 0;
    let mut registers = [0u64; 8];
    let mut custom_registers = false;
    let mut used = [false; 9];
    let mut usb_uart = false;
    let mut usb_control = false;
    let mut usb_net = false;
    let mut require_upload = false;
    let mut cursor = 5;
    while cursor < args.len() {
        if args[cursor] == "--usb-uart" {
            if usb_uart {
                return Err("duplicate payload option --usb-uart".into());
            }
            usb_uart = true;
            cursor += 1;
            continue;
        }
        if args[cursor] == "--usb-control" {
            if usb_control {
                return Err("duplicate payload option --usb-control".into());
            }
            usb_control = true;
            cursor += 1;
            continue;
        }
        if args[cursor] == "--usb-net" {
            if usb_net {
                return Err("duplicate payload option --usb-net".into());
            }
            usb_net = true;
            cursor += 1;
            continue;
        }
        if args[cursor] == "--no-fallback" {
            if require_upload {
                return Err("duplicate payload option --no-fallback".into());
            }
            require_upload = true;
            cursor += 1;
            continue;
        }
        let pair = args.get(cursor..cursor + 2).ok_or(crate::USAGE)?;
        let flag = pair[0].to_str().ok_or("option is not UTF-8")?;
        let index = match flag {
            "--entry-offset" => 8,
            "--x0" => 0,
            "--x1" => 1,
            "--x2" => 2,
            "--x3" => 3,
            "--x4" => 4,
            "--x5" => 5,
            "--x6" => 6,
            "--x7" => 7,
            _ => return Err(format!("unknown payload option {flag}")),
        };
        if used[index] {
            return Err(format!("duplicate payload option {flag}"));
        }
        used[index] = true;
        let value = number(&pair[1])?;
        if index == 8 {
            entry_offset = value;
        } else {
            registers[index] = value;
            custom_registers = true;
        }
        cursor += 2;
    }
    if require_upload && !(usb_uart || usb_control || usb_net) {
        return Err("--no-fallback requires --usb-control, --usb-uart or --usb-net".into());
    }
    let runtime_size = number(&args[3])?;
    let raw = read(Path::new(&args[0]))?;
    let bootstrap_version = raw.get(40..48);
    if !matches!(bootstrap_version, Some(b"SVBOOT04" | b"SVBOOT05"))
        || field(&raw, 8)? != RESIDENT_BASE
        || field(&raw, 16)? != raw.len() as u64
        || field(&raw, 24)? < raw.len() as u64
        || field(&raw, 32)? < field(&raw, 24)?
        || field(&raw, 32)? > 1024 * 1024
    {
        return Err(
            "use a payload bootstrap linked at the fixed resident base (scripts/build-payload.sh)"
                .into(),
        );
    }
    if usb_net && bootstrap_version != Some(b"SVBOOT05".as_slice()) {
        return Err("--usb-net requires a rebuilt bootstrap (scripts/build-payload.sh)".into());
    }
    let slot = raw
        .get(CONFIG_OFFSET..CONFIG_OFFSET + CONFIG_SIZE)
        .ok_or("bootstrap has no payload descriptor slot")?;
    if slot.iter().any(|byte| *byte != 0) {
        return Err("bootstrap payload descriptor is already populated".into());
    }
    let directory = Path::new(&args[1]);
    let bootstack = inspect_bootstack(directory)?;
    let original = read(&directory.join("bl33.bin"))?;
    if Some(digest(&original).as_str()) != bootstack["files"]["bl33.bin"]["sha256"].as_str() {
        return Err("bl33.bin changed after bootstack verification".into());
    }
    let original_image = UbootImage::parse(&original).map_err(|e| e.to_string())?;
    let start = original_image.end_offset as usize;
    let probe = &original[start..start + original_image.control_fdt_size];
    let input = Path::new(&args[2]);
    if fs::metadata(input)
        .map_err(|e| format!("{}: {e}", input.display()))?
        .len()
        > MAX_PACKAGE_SIZE
    {
        return Err("payload exceeds package size limit".into());
    }
    let data = read(input)?;
    let mut bytes = raw.clone();
    bytes.extend_from_slice(probe);
    bytes.resize((bytes.len() + 15) & !15, 0);
    let offset = bytes.len() as u64;
    let payload = Payload {
        package_size: offset
            .checked_add(data.len() as u64)
            .ok_or("package size overflow")?,
        bootstrap_size: raw.len() as u64,
        offset,
        file_size: data.len() as u64,
        runtime_size,
        entry_offset,
        registers,
        preserve_boot_args: !custom_registers,
        usb_uart,
        usb_control,
        usb_net,
        require_upload,
        crc32: crc32(&data),
    };
    let descriptor = payload.encode().map_err(|e| e.to_string())?;
    bytes.extend_from_slice(&data);
    bytes[CONFIG_OFFSET..CONFIG_OFFSET + CONFIG_SIZE].copy_from_slice(&descriptor);
    payload.source(&bytes).map_err(|e| e.to_string())?;
    if Payload::decode(&bytes[CONFIG_OFFSET..CONFIG_OFFSET + CONFIG_SIZE])
        .map_err(|e| e.to_string())?
        != Some(payload)
    {
        return Err("payload descriptor roundtrip failed".into());
    }
    let image = UbootImage::parse(&bytes).map_err(|e| e.to_string())?;
    image
        .validate_hekate_patches(&bytes)
        .map_err(|e| e.to_string())?;
    for port in 0..=3 {
        let mut patched = bytes.clone();
        image
            .apply_hekate_uart_patch(&mut patched, port)
            .map_err(|e| e.to_string())?;
        payload.source(&patched).map_err(|e| e.to_string())?;
    }
    // Validate completely before creating output; Native assets and input files cannot be overwritten.
    let output = Path::new(&args[4]);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|e| format!("{}: {e}", output.display()))?;
    file.write_all(&bytes)
        .map_err(|e| format!("{}: {e}", output.display()))?;
    Ok(json!({
        "kind":"el1-raw-payload", "hardware_validated":false, "stage2_enabled":true,
        "stage2":{"ipa_equals_pa":true,"ipa_bits":36,"resident_size_bytes":RESIDENT_SIZE,
            "physical_interrupts":"vgicv2","cpu_count":switchvisor::psci::CPU_COUNT,
            "mc_trap_base":format!("{:#x}",switchvisor::mc::BASE), "virtual_carveout":"gsc5"},
        "output":output.display().to_string(), "sha256":digest(&bytes), "file_size_bytes":bytes.len(),
        "bootstrap":{"load_base":format!("{LOAD_BASE:#x}"), "resident_base":format!("{RESIDENT_BASE:#x}"),
            "file_size_bytes":raw.len(), "runtime_size_bytes":field(&raw,32)?, "raw_sha256":digest(&raw)},
        "payload":{"input":input.display().to_string(), "sha256":digest(&data), "offset":offset,
            "file_size_bytes":data.len(), "runtime_size_bytes":runtime_size,
            "load_base":format!("{LOAD_BASE:#x}"), "entry":format!("{:#x}",payload.entry().map_err(|e|e.to_string())?),
            "stack_top":format!("{STACK_TOP:#x}"), "preserve_boot_args":payload.preserve_boot_args, "registers":registers},
        "probe_fdt_offset":raw.len(), "probe_fdt_sha256":digest(probe), "bootstack":bootstack,
        "usb_uart":{"enabled":usb_uart,"compatible":"ns16550a",
            "gpa":format!("{:#x}",switchvisor::vdev::uart::BASE),
            "interrupt":switchvisor::vdev::uart::INTERRUPT_ID,
            "transport":"cdc-acm","vid":switchvisor::drivers::usb::cdc::VID,
            "pid":switchvisor::drivers::usb::cdc::PID},
        "usb_net":{"enabled":usb_net,"transport":"cdc-ncm","compatible":"virtio,mmio",
            "gpa":format!("{:#x}",switchvisor::vdev::virtio_net::BASE),
            "interrupt":switchvisor::vdev::virtio_net::INTERRUPT_ID,"mmio_version":2,
            "guest_mac":"02:53:56:00:00:02","host_mac":"02:53:56:00:00:01",
            "management_mac":"02:53:56:00:00:03","management_ip":"192.168.77.1",
            "management_udp_port":7777,"mtu":1500},
        "usb_control":{"enabled":usb_control || usb_uart || usb_net,"control":"cdc-acm","loader":"vendor-bulk",
            "packaged_payload_fallback":!require_upload}
    }))
}
