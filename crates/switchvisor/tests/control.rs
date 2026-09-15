use switchvisor::control::{Command, ParseError, Parser};

fn parse(bytes: &[u8]) -> Vec<Result<Command, ParseError>> {
    let mut parser = Parser::new();
    bytes.iter().filter_map(|&byte| parser.push(byte)).collect()
}

#[test]
fn commands_accept_lf_or_crlf_and_ignore_empty_lines() {
    assert_eq!(
        parse(b"\nping\nstatus\r\nreboot\nreboot-rcm\r\n"),
        [
            Ok(Command::Ping),
            Ok(Command::Status),
            Ok(Command::Reboot),
            Ok(Command::RebootRcm),
        ]
    );
}

#[test]
fn malformed_lines_are_bounded_and_recovery_starts_at_the_next_line() {
    let mut input = vec![b'x'; 65];
    input.extend_from_slice(b"\nping\nunknown\ninvalid\0line\nstatus\n");
    assert_eq!(
        parse(&input),
        [
            Err(ParseError::LineTooLong),
            Ok(Command::Ping),
            Err(ParseError::UnknownCommand),
            Err(ParseError::InvalidEncoding),
            Ok(Command::Status),
        ]
    );
}
