//! `rndis-tetherctl` — asks the daemon for its status.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

const SOCKET_PATH: &str = "/var/run/rndis-tether.sock";

fn main() -> ExitCode {
    let command = std::env::args().nth(1).unwrap_or_else(|| "status".into());
    if command == "--help" || command == "-h" {
        println!("usage: rndis-tetherctl [status]");
        return ExitCode::SUCCESS;
    }

    match query(&command) {
        Ok(reply) => {
            print(&reply);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rndis-tetherctl: {e}");
            eprintln!("is rndis-tetherd running?");
            ExitCode::FAILURE
        }
    }
}

fn query(command: &str) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    writeln!(stream, "{command}")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply)
}

/// Turn the daemon's `key=value` line into readable output.
fn print(reply: &str) {
    for field in reply.trim().split(' ') {
        match field.split_once('=') {
            Some((key, value)) => println!("{key:<10} {value}"),
            None => println!("{field}"),
        }
    }
}
