use clap::{Args, Parser, Subcommand};
use scarlet_switchvisor::{Operation, Words};
use std::{
    io::{self, Write},
    process::ExitCode,
    thread,
    time::Duration,
};

#[derive(Debug, Parser)]
#[command(about = "Read Switchvisor logs and control guest MMIO tracing")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, PartialEq, Eq)]
struct ReadOptions {
    /// Begin reading at this cursor.
    #[arg(long, value_name = "N", default_value_t = 0, value_parser = number)]
    cursor: u64,

    /// Keep reading as records arrive.
    #[arg(long)]
    follow: bool,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Read the EL2 diagnostic log.
    Log(ReadOptions),
    /// Control MMIO tracing.
    #[command(subcommand)]
    Trace(TraceCommand),
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum TraceCommand {
    /// Begin tracing a guest physical address range.
    Start {
        #[arg(value_name = "IPA", value_parser = number)]
        address: u64,
        #[arg(value_parser = number)]
        length: u64,
    },
    /// Stop tracing and retain the records.
    Stop,
    /// Read retained MMIO trace records.
    Read(ReadOptions),
}

fn number(value: &str) -> Result<u64, &'static str> {
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    }
    .map_err(|_| "expected an unsigned decimal or hexadecimal integer")
}

#[cfg(all(target_arch = "aarch64", target_os = "scarlet"))]
struct Control(std::fs::File);

#[cfg(all(target_arch = "aarch64", target_os = "scarlet"))]
impl Control {
    fn open() -> Result<Self, String> {
        let path = scarlet_switchvisor::DEVICE_PATH;
        std::fs::File::open(path)
            .map(Self)
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => {
                    format!("open {path}: device is absent in this boot profile")
                }
                _ => format!("open {path}: {error}"),
            })
    }

    fn request(&self, operation: Operation, arg0: u64, arg1: u64) -> Result<Words, &'static str> {
        use scarlet_switchvisor as wire;
        use std::os::fd::AsRawFd;

        let mut words = [arg0, arg1, 0, 0, 0, 0, 0, 0];
        let address = if operation == Operation::StopTrace {
            0
        } else {
            words.as_mut_ptr() as usize
        };
        // SAFETY: The control call is synchronous and receives a writable
        // Words buffer for every command except StopTrace.
        let status = unsafe {
            scarlet_sys::syscall3(
                scarlet_sys::Syscall::HandleControl,
                self.0.as_raw_fd() as usize,
                operation as usize,
                address,
            )
        };
        match i32::try_from(status) {
            Ok(wire::OK) => Ok(words),
            Ok(wire::PERMISSION_DENIED) => {
                Err("this Scarlet account is not authorized for Switchvisor")
            }
            Ok(wire::BAD_BUFFER) => Err("Scarlet could not access the Switchvisor request buffer"),
            Ok(wire::UNSUPPORTED) => Err("the running Switchvisor does not support this operation"),
            Ok(wire::INVALID_ARGUMENT) => Err("Switchvisor rejected the range"),
            Ok(wire::BUSY) => Err("an MMIO capture is already active"),
            _ => Err("Switchvisor device control failed"),
        }
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "scarlet")))]
struct Control;

#[cfg(not(all(target_arch = "aarch64", target_os = "scarlet")))]
impl Control {
    fn open() -> Result<Self, String> {
        Err("run this command in the AArch64 Scarlet guest".into())
    }

    fn request(&self, _: Operation, _: u64, _: u64) -> Result<Words, &'static str> {
        unreachable!("device is only available on AArch64 Scarlet")
    }
}

fn write_log(output: &mut impl Write, reply: Words) -> io::Result<()> {
    let mut bytes = [0; 48];
    for (word, chunk) in reply[2..].iter().zip(bytes.chunks_exact_mut(8)) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    output.write_all(&bytes[..reply[0] as usize])
}

fn write_event(output: &mut impl Write, reply: Words) -> io::Result<()> {
    let [_, next, address, pc, value, flags, ..] = reply;
    let cpu = (flags >> 8) & 0xff;
    let cursor = next - 1;
    write!(
        output,
        "{cursor} cpu={cpu} ipa={address:#010x} pc={pc:#018x}"
    )?;
    if flags & (1 << 18) != 0 {
        return writeln!(output, " capture-stopped");
    }
    let direction = if flags & (1 << 16) != 0 {
        "write"
    } else {
        "read"
    };
    let size = flags & 0xff;
    write!(output, " {direction} bytes={size}")?;
    if flags & (1 << 17) != 0 {
        writeln!(output, " value={value:#x}")
    } else {
        writeln!(output, " value=unavailable")
    }
}

fn read(control: &Control, operation: Operation, options: ReadOptions) -> Result<(), String> {
    let mut cursor = options.cursor;
    let mut output = io::stdout().lock();
    loop {
        let reply = control.request(operation, cursor, 0)?;
        let [count, next, ..] = reply;
        if count == 0 {
            output.flush().map_err(|error| error.to_string())?;
            if !options.follow {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(250));
            continue;
        }

        let cap = if operation == Operation::ReadLog {
            48
        } else {
            1
        };
        let start = next.checked_sub(count).filter(|&start| start >= cursor);
        let Some(start) = start.filter(|_| count <= cap) else {
            return Err("invalid cursor or count in Switchvisor reply".into());
        };
        if start > cursor {
            eprintln!("swvctl: cursor advanced from {cursor} to {start}");
        }
        if operation == Operation::ReadLog {
            write_log(&mut output, reply)
        } else {
            write_event(&mut output, reply)
        }
        .map_err(|error| error.to_string())?;
        cursor = next;
    }
}

fn run(command: Command) -> Result<(), String> {
    let control = Control::open()?;
    match command {
        Command::Log(options) => return read(&control, Operation::ReadLog, options),
        Command::Trace(TraceCommand::Read(options)) => {
            return read(&control, Operation::ReadTrace, options);
        }
        Command::Trace(TraceCommand::Start { address, length }) => {
            control.request(Operation::StartTrace, address, length)?;
            println!("MMIO capture started at {address:#x} for {length} bytes");
        }
        Command::Trace(TraceCommand::Stop) => {
            control.request(Operation::StopTrace, 0, 0)?;
            println!("MMIO capture stopped");
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("swvctl: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trace_and_log_options() {
        assert_eq!(
            Cli::try_parse_from(["swvctl", "trace", "start", "0x70019100", "0X4"])
                .unwrap()
                .command,
            Command::Trace(TraceCommand::Start {
                address: 0x7001_9100,
                length: 4
            })
        );
        assert_eq!(
            Cli::try_parse_from(["swvctl", "trace", "read", "--follow", "--cursor", "12"])
                .unwrap()
                .command,
            Command::Trace(TraceCommand::Read(ReadOptions {
                cursor: 12,
                follow: true
            }))
        );
        assert!(Cli::try_parse_from(["swvctl", "trace", "start", "0x", "4"]).is_err());
        assert!(Cli::try_parse_from(["swvctl", "log", "--cursor", "0", "--cursor", "1"]).is_err());
    }

    #[test]
    fn renders_raw_log_and_interrupted_capture() {
        let mut output = Vec::new();
        let mut reply = [0; 8];
        reply[0] = 3;
        reply[2] = u64::from_le_bytes(*b"EL2\0\0\0\0\0");
        write_log(&mut output, reply).unwrap();
        assert_eq!(output, b"EL2");
        output.clear();
        write_event(
            &mut output,
            [1, 3, 0x7000_0000, 0x8020_0000, 0, 1 << 18, 0, 0],
        )
        .unwrap();
        assert_eq!(
            output,
            b"2 cpu=0 ipa=0x70000000 pc=0x0000000080200000 capture-stopped\n"
        );
    }
}
