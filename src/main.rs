//! `muta` — Android USB tethering for macOS, in userspace.
//!
//! macOS ships no RNDIS driver, so the phone's RNDIS interface sits unclaimed
//! and an ordinary process can take it via IOKit: no kext, no System Extension,
//! no SIP changes.

use anyhow::{bail, Result};

mod daemon;
mod device;
mod link;
mod netstack;
mod probe;
mod rndis;
mod service;
mod signals;
mod status;
mod transport;
mod tun;
mod tunnel;
mod usb;

const USAGE: &str = "\
muta — Android USB tethering for macOS

usage:
  muta run [-v]      bring up tethering in the foreground (needs root)
  muta status        show the current connection
  muta probe         dump USB descriptors of any attached RNDIS device
  muta install       install and start the background service (needs root)
  muta uninstall     stop and remove the background service (needs root)
  muta --version     print the version

Try `sudo muta run -v` first. Install once you want it to survive reboots.
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    match command {
        "run" => {
            init_logging(args.iter().any(|a| a == "-v" || a == "--verbose"));
            require_root("run")?;
            daemon::run()
        }
        "status" => status::print_status(),
        "probe" => {
            init_logging(args.iter().any(|a| a == "-v" || a == "--verbose"));
            probe::run(args.iter().any(|a| a == "--watch"))
        }
        "install" => {
            init_logging(false);
            require_root("install")?;
            service::install()
        }
        "uninstall" => {
            init_logging(false);
            require_root("uninstall")?;
            service::uninstall()
        }
        "--version" | "-V" => {
            println!("muta {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => {
            eprint!("muta: unknown command `{other}`\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn init_logging(verbose: bool) {
    // nusb logs every transfer; keep it out of our own output.
    let default = if verbose {
        "debug,nusb=info"
    } else {
        "info,nusb=warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
        .format_timestamp_millis()
        .init();
}

/// utun, routes, DNS and `/Library/LaunchDaemons` all need root. Fail here
/// rather than part-way through a bring-up. `status` deliberately does not.
fn require_root(command: &str) -> Result<()> {
    // SAFETY: geteuid cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        bail!("`muta {command}` must run as root (try: sudo muta {command})");
    }
    Ok(())
}
