mod bundle;

use bundle::PreparedBundle;
use nusb::{
    Endpoint, MaybeFuture,
    transfer::{Buffer, Bulk, In, Out},
};
use serialport::SerialPortType;
use std::{
    env,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};
use switchvisor::{
    drivers::usb::cdc::{PID, VID},
    loader::{
        BUNDLE_DESCRIPTOR_SIZE, GUEST_RAM_BASE, GUEST_RAM_END, HEADER_SIZE, Header,
        IMAGE_DESCRIPTOR_SIZE, MAX_BODY_SIZE, MAX_IMAGES, MAX_RESPONSE_SIZE, Opcode,
        PRESERVE_BOOT_ARGS, REPLY_FLAG, StatusCode, VERSION,
    },
    payload::{LOAD_BASE, MAX_RUNTIME_SIZE},
};

const TIMEOUT: Duration = Duration::from_secs(3);
const DEVICE_WAIT: Duration = Duration::from_secs(15);
const LOADER_INTERFACE: u8 = 4;
const LOADER_OUT: u8 = 0x05;
const LOADER_IN: u8 = 0x85;
const USAGE: &str = "Usage:\n  switchvisorctl [--port <serial-device>] <ping|status|reboot|reboot-rcm>\n  switchvisorctl deploy <bundle-directory|bundle.json>\n  switchvisorctl upload-bl33 <payload.raw> --runtime-size <size> [--entry-offset <size>] [--x0 <value> ... --x7 <value>]\n  switchvisorctl <hello|loader-status|boot|abort>\n\nNumbers accept decimal or 0x-prefixed hexadecimal notation. SWITCHVISOR_CONTROL_PORT may supply the control serial device.";

fn number(value: &str) -> Result<u64, String> {
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    }
    .map_err(|error| format!("invalid number {value:?}: {error}"))
}

fn control_port(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(path) = env::var_os("SWITCHVISOR_CONTROL_PORT") {
        return Ok(path.into());
    }
    let mut candidates = Vec::new();
    for port in serialport::available_ports().map_err(|error| error.to_string())? {
        #[cfg(target_os = "macos")]
        if !port.port_name.starts_with("/dev/cu.") {
            continue;
        }
        let SerialPortType::UsbPort(info) = port.port_type else {
            continue;
        };
        if info.vid == VID && info.pid == PID && matches!(info.interface, Some(2 | 3)) {
            candidates.push(PathBuf::from(port.port_name));
        }
    }
    match candidates.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err("Switchvisor control CDC port not found; pass --port <serial-device>".into()),
        _ => {
            Err("multiple Switchvisor control CDC ports found; pass --port <serial-device>".into())
        }
    }
}

fn control(command: &str, explicit: Option<PathBuf>) -> Result<(), String> {
    let path = control_port(explicit)?;
    let name = path
        .to_str()
        .ok_or_else(|| format!("serial path is not UTF-8: {}", path.display()))?;
    let mut port = serialport::new(name, 115_200)
        .timeout(TIMEOUT)
        .open()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    port.write_data_terminal_ready(true)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    port.write_all(command.as_bytes())
        .and_then(|_| port.write_all(b"\n"))
        .and_then(|_| port.flush())
        .map_err(|error| format!("{}: {error}", path.display()))?;

    let mut reply = Vec::new();
    let mut buffer = [0; 256];
    loop {
        match port.read(&mut buffer) {
            Ok(0) => continue,
            Ok(length) => {
                reply.extend_from_slice(&buffer[..length]);
                if reply.len() > 4096 {
                    return Err("control reply exceeds 4096 bytes".into());
                }
                if let Some(final_line) = reply
                    .split_inclusive(|byte| *byte == b'\n')
                    .rfind(|line| line.ends_with(b"\n"))
                {
                    let line = final_line.strip_suffix(b"\n").unwrap_or(final_line);
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                    if line == b"OK" || line.starts_with(b"OK ") {
                        print!("{}", String::from_utf8_lossy(&reply));
                        return Ok(());
                    }
                    if line == b"ERR" || line.starts_with(b"ERR ") {
                        print!("{}", String::from_utf8_lossy(&reply));
                        return Err("Switchvisor rejected the control command".into());
                    }
                }
            }
            Err(error)
                if matches!(command, "reboot" | "reboot-rcm")
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                    ) =>
            {
                if !reply.is_empty() {
                    print!("{}", String::from_utf8_lossy(&reply));
                }
                return Ok(());
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
}

struct LoaderTransport {
    output: Endpoint<Bulk, Out>,
    input: Endpoint<Bulk, In>,
    request_id: u32,
}

struct Capabilities {
    guest_ram_base: u64,
    guest_ram_end: u64,
    max_chunk_size: usize,
    bundle_descriptor_size: usize,
    image_descriptor_size: usize,
    max_images: usize,
}

fn body_u32(body: &[u8], offset: usize) -> Result<u32, String> {
    let bytes = body
        .get(offset..offset + 4)
        .ok_or("truncated loader response")?;
    Ok(u32::from_le_bytes(
        bytes.try_into().map_err(|_| "truncated loader response")?,
    ))
}

fn body_u64(body: &[u8], offset: usize) -> Result<u64, String> {
    let bytes = body
        .get(offset..offset + 8)
        .ok_or("truncated loader response")?;
    Ok(u64::from_le_bytes(
        bytes.try_into().map_err(|_| "truncated loader response")?,
    ))
}

fn capabilities(body: &[u8]) -> Result<Capabilities, String> {
    if body.len() != 32 {
        return Err(format!("invalid HELLO response body size {}", body.len()));
    }
    Ok(Capabilities {
        guest_ram_base: body_u64(body, 0)?,
        guest_ram_end: body_u64(body, 8)?,
        max_chunk_size: body_u32(body, 16)? as usize,
        bundle_descriptor_size: body_u32(body, 20)? as usize,
        image_descriptor_size: body_u32(body, 24)? as usize,
        max_images: body_u32(body, 28)? as usize,
    })
}

impl LoaderTransport {
    fn open() -> Result<Self, String> {
        let deadline = Instant::now() + DEVICE_WAIT;
        let mut last_error = None;
        loop {
            let mut matches = nusb::list_devices()
                .wait()
                .map_err(|error| error.to_string())?
                .filter(|device| device.vendor_id() == VID && device.product_id() == PID);
            if let Some(info) = matches.next() {
                if matches.next().is_some() {
                    return Err("multiple Switchvisor USB devices found".into());
                }
                let opened = info
                    .open()
                    .wait()
                    .map_err(|error| error.to_string())
                    .and_then(|device| {
                        device
                            .claim_interface(LOADER_INTERFACE)
                            .wait()
                            .map_err(|error| format!("cannot claim loader interface: {error}"))
                    });
                match opened {
                    Ok(interface) => {
                        let output = interface
                            .endpoint::<Bulk, Out>(LOADER_OUT)
                            .map_err(|error| error.to_string())?;
                        let input = interface
                            .endpoint::<Bulk, In>(LOADER_IN)
                            .map_err(|error| error.to_string())?;
                        return Ok(Self {
                            output,
                            input,
                            request_id: 1,
                        });
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            if Instant::now() >= deadline {
                return Err(last_error.unwrap_or_else(|| {
                    "Switchvisor USB device was not found within 15 seconds".into()
                }));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn request(
        &mut self,
        opcode: Opcode,
        arg0: u64,
        body: &[u8],
    ) -> Result<(Header, Vec<u8>), String> {
        if body.len() > MAX_BODY_SIZE {
            return Err("loader request body is too large".into());
        }
        let request_id = self.request_id;
        self.request_id = self.request_id.wrapping_add(1).max(1);
        let mut header = Header::request(opcode, request_id, body.len() as u32);
        header.arg0 = arg0;
        let mut encoded_header = [0; HEADER_SIZE];
        header.encode(&mut encoded_header);
        let mut message = Vec::with_capacity(HEADER_SIZE + body.len());
        message.extend_from_slice(&encoded_header);
        message.extend_from_slice(body);

        let packet = self.output.max_packet_size();
        let needs_zlp = message.len() < HEADER_SIZE + MAX_BODY_SIZE && message.len() % packet == 0;
        let completion = self.output.transfer_blocking(message.into(), TIMEOUT);
        completion
            .status
            .map_err(|error| format!("loader OUT transfer failed: {error}"))?;
        if needs_zlp {
            let completion = self
                .output
                .transfer_blocking(Vec::<u8>::new().into(), TIMEOUT);
            completion
                .status
                .map_err(|error| format!("loader OUT ZLP failed: {error}"))?;
        }

        let response_size = 512.max(self.input.max_packet_size());
        let completion = self
            .input
            .transfer_blocking(Buffer::new(response_size), TIMEOUT);
        completion
            .status
            .map_err(|error| format!("loader IN transfer failed: {error}"))?;
        let response = completion.buffer.into_vec();
        if response.len() < HEADER_SIZE || response.len() > MAX_RESPONSE_SIZE {
            return Err(format!("invalid loader response size {}", response.len()));
        }
        let response_header = Header::decode(&response).map_err(|error| format!("{error:?}"))?;
        if response_header.request_id != request_id
            || response_header.opcode != (opcode as u16 | REPLY_FLAG)
        {
            return Err("loader response does not match its request".into());
        }
        let body = response[HEADER_SIZE..].to_vec();
        if response_header.arg0 != StatusCode::Ok as u64 {
            return Err(format!(
                "loader rejected {:?}: status={} state={}",
                opcode, response_header.arg0, response_header.arg1
            ));
        }
        Ok((response_header, body))
    }
}

fn validate_capabilities(
    capabilities: &Capabilities,
    bundle: &PreparedBundle,
) -> Result<(), String> {
    if capabilities.guest_ram_base != GUEST_RAM_BASE
        || capabilities.guest_ram_end != GUEST_RAM_END
        || capabilities.bundle_descriptor_size != BUNDLE_DESCRIPTOR_SIZE
        || capabilities.image_descriptor_size != IMAGE_DESCRIPTOR_SIZE
        || capabilities.max_chunk_size == 0
        || capabilities.max_chunk_size > MAX_BODY_SIZE
        || capabilities.max_images > MAX_IMAGES
        || bundle.images.len() > capabilities.max_images
    {
        return Err("loader capabilities do not match the bundle contract".into());
    }
    Ok(())
}

fn transfer_bundle(bundle: &PreparedBundle, boot: bool) -> Result<(), String> {
    let mut transport = LoaderTransport::open()?;
    let (_, hello) = transport.request(Opcode::Hello, 0, &[])?;
    let capabilities = capabilities(&hello)?;
    validate_capabilities(&capabilities, bundle)?;

    let mut descriptor = [0; BUNDLE_DESCRIPTOR_SIZE];
    bundle.descriptor.encode(&mut descriptor);
    transport.request(Opcode::BeginBundle, 0, &descriptor)?;
    let transfer = (|| {
        for (index, image) in bundle.images.iter().enumerate() {
            let mut descriptor = [0; IMAGE_DESCRIPTOR_SIZE];
            image.descriptor.encode(&mut descriptor);
            transport.request(Opcode::BeginImage, 0, &descriptor)?;

            let mut file = File::open(&image.path)
                .map_err(|error| format!("{}: {error}", image.path.display()))?;
            let mut buffer = vec![0; capabilities.max_chunk_size];
            let mut offset = 0u64;
            let mut last_progress = Instant::now();
            loop {
                let count = file
                    .read(&mut buffer)
                    .map_err(|error| format!("{}: {error}", image.path.display()))?;
                if count == 0 {
                    break;
                }
                let (reply, _) = transport.request(Opcode::Data, offset, &buffer[..count])?;
                offset += count as u64;
                if reply.arg1 != offset {
                    return Err(format!(
                        "loader acknowledged {} bytes after sending {offset}",
                        reply.arg1
                    ));
                }
                if last_progress.elapsed() >= Duration::from_secs(1)
                    || offset == image.descriptor.file_size
                {
                    eprintln!(
                        "image {}/{} {}: {offset}/{} bytes",
                        index + 1,
                        bundle.images.len(),
                        image.path.display(),
                        image.descriptor.file_size
                    );
                    last_progress = Instant::now();
                }
            }
            if offset != image.descriptor.file_size {
                return Err(format!("{} changed while uploading", image.path.display()));
            }
            transport.request(Opcode::EndImage, 0, &[])?;
        }
        transport.request(Opcode::CommitBundle, 0, &[])?;
        if boot {
            transport.request(Opcode::Boot, 0, &[])?;
        }
        Ok(())
    })();
    if let Err(error) = transfer {
        let _ = transport.request(Opcode::Abort, 0, &[]);
        return Err(error);
    }
    println!(
        "{}: images={} entry={:#x}",
        if boot { "booted" } else { "ready" },
        bundle.images.len(),
        bundle.descriptor.entry
    );
    Ok(())
}

fn upload_bl33(path: &Path, args: &[String]) -> Result<(), String> {
    let mut runtime_size = None;
    let mut entry_offset = 0;
    let mut registers = [0; 8];
    let mut explicit_registers = false;
    let mut used = [false; 10];
    let mut cursor = 0;
    while cursor < args.len() {
        let flag = &args[cursor];
        let value = args.get(cursor + 1).ok_or(USAGE)?;
        let index = match flag.as_str() {
            "--runtime-size" => 8,
            "--entry-offset" => 9,
            "--x0" => 0,
            "--x1" => 1,
            "--x2" => 2,
            "--x3" => 3,
            "--x4" => 4,
            "--x5" => 5,
            "--x6" => 6,
            "--x7" => 7,
            _ => return Err(format!("unknown upload option {flag}")),
        };
        if used[index] {
            return Err(format!("duplicate upload option {flag}"));
        }
        used[index] = true;
        let value = number(value)?;
        match index {
            8 => runtime_size = Some(value),
            9 => entry_offset = value,
            register => {
                registers[register] = value;
                explicit_registers = true;
            }
        }
        cursor += 2;
    }
    let runtime_size = runtime_size.ok_or("--runtime-size is required")?;
    if runtime_size > MAX_RUNTIME_SIZE {
        return Err("payload runtime exceeds the 64 MiB BL33 limit".into());
    }
    let entry = LOAD_BASE
        .checked_add(entry_offset)
        .ok_or("payload entry overflows u64")?;
    let bundle = PreparedBundle::single(
        path,
        LOAD_BASE,
        runtime_size,
        entry,
        if explicit_registers {
            0
        } else {
            PRESERVE_BOOT_ARGS
        },
        registers,
    )?;
    transfer_bundle(&bundle, false)
}

fn deploy(path: &Path) -> Result<(), String> {
    let bundle = PreparedBundle::from_manifest(path)?;
    transfer_bundle(&bundle, true)
}

fn loader_command(opcode: Opcode) -> Result<(), String> {
    let mut transport = LoaderTransport::open()?;
    let (header, body) = transport.request(opcode, 0, &[])?;
    match opcode {
        Opcode::Hello => {
            let capabilities = capabilities(&body)?;
            println!(
                "protocol={} guest-ram={:#x}..{:#x} max-chunk={} max-images={}",
                VERSION,
                capabilities.guest_ram_base,
                capabilities.guest_ram_end,
                capabilities.max_chunk_size,
                capabilities.max_images
            );
        }
        Opcode::Status if body.len() == 48 => {
            println!(
                "state={} claimed={} received={} images={}/{} current={:#x} file-size={} runtime-size={}",
                body_u32(&body, 0)?,
                body_u32(&body, 4)?,
                body_u64(&body, 8)?,
                body_u32(&body, 20)?,
                body_u32(&body, 16)?,
                body_u64(&body, 24)?,
                body_u64(&body, 32)?,
                body_u64(&body, 40)?
            );
        }
        Opcode::Status => {
            return Err(format!("invalid STATUS response body size {}", body.len()));
        }
        _ if body.is_empty() => println!("OK state={}", header.arg1),
        _ => return Err("unexpected loader response body".into()),
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(());
    }
    let mut port = None;
    if args.first().is_some_and(|value| value == "--port") {
        if args.len() < 3 {
            return Err(USAGE.into());
        }
        port = Some(PathBuf::from(args.remove(1)));
        args.remove(0);
    }
    match args[0].as_str() {
        command @ ("ping" | "status" | "reboot" | "reboot-rcm") if args.len() == 1 => {
            control(command, port)
        }
        "deploy" if args.len() == 2 && port.is_none() => deploy(Path::new(&args[1])),
        "upload-bl33" if args.len() >= 2 && port.is_none() => {
            upload_bl33(Path::new(&args[1]), &args[2..])
        }
        "hello" if args.len() == 1 && port.is_none() => loader_command(Opcode::Hello),
        "loader-status" if args.len() == 1 && port.is_none() => loader_command(Opcode::Status),
        "boot" if args.len() == 1 && port.is_none() => loader_command(Opcode::Boot),
        "abort" if args.len() == 1 && port.is_none() => loader_command(Opcode::Abort),
        _ => Err(USAGE.into()),
    }
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
