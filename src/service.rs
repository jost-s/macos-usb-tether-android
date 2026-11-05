//! Installing and removing the LaunchDaemon.
//!
//! The daemon runs as root, so its binary must live somewhere only root can
//! write. `/usr/local/bin` is group-writable by `admin` on stock macOS, which
//! would let any admin-level process replace the binary and gain root at the
//! next launch — hence the dedicated root-owned directory below.

use std::ffi::{CStr, CString};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use log::{info, warn};

use crate::signals;
use crate::status::SOCKET_PATH;

pub const LABEL: &str = "com.github.jost-s.muta";
const INSTALL_DIR: &str = "/usr/local/libexec/muta";
const PLIST_PATH: &str = "/Library/LaunchDaemons/com.github.jost-s.muta.plist";
const NEWSYSLOG_PATH: &str = "/etc/newsyslog.d/muta.conf";
pub const LOG_PATH: &str = "/var/log/muta.log";
pub const LOG_PATH_C: &CStr = c"/var/log/muta.log";
/// Where newsyslog finds the daemon to signal after rotating the log.
const PID_PATH: &str = "/var/run/muta.pid";

fn installed_binary() -> PathBuf {
    Path::new(INSTALL_DIR).join("muta")
}

pub fn install() -> Result<()> {
    let source = std::env::current_exe().context("locating the running executable")?;
    let target = installed_binary();

    // Reinstalling over a running service: stop it before replacing the binary.
    if Path::new(PLIST_PATH).exists() {
        info!("stopping the running service");
        let _ = launchctl(&["bootout", &format!("system/{LABEL}")]);
    }

    root_owned_dir(Path::new(INSTALL_DIR))?;

    if source.canonicalize().ok() != target.canonicalize().ok() {
        // Write beside the target and rename, so a failed copy cannot leave a
        // half-written binary that launchd would try to run.
        let staged = target.with_extension("new");
        std::fs::copy(&source, &staged)
            .with_context(|| format!("copying {} to {}", source.display(), staged.display()))?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        chown_root(&staged)?;
        std::fs::rename(&staged, &target)?;
    }
    info!("installed {}", target.display());

    write_root_file(Path::new(PLIST_PATH), &plist(&target), 0o644)?;
    write_root_file(Path::new(NEWSYSLOG_PATH), NEWSYSLOG_CONF, 0o644)?;

    bootstrap()?;
    // Enable is separate: a previous `bootout` can leave the label disabled.
    let _ = launchctl(&["enable", &format!("system/{LABEL}")]);

    info!("service loaded; it will start again at every boot");
    println!("installed. enable USB tethering on the phone, then:");
    println!("  muta status");
    println!("  tail -f {LOG_PATH}");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    if Path::new(PLIST_PATH).exists() {
        let _ = launchctl(&["bootout", &format!("system/{LABEL}")]);
    }

    for path in [
        PLIST_PATH,
        NEWSYSLOG_PATH,
        LOG_PATH,
        PID_PATH,
        SOCKET_PATH,
        &installed_binary().to_string_lossy(),
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => info!("removed {path}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("removing {path}: {e}"),
        }
    }
    // Only succeeds when empty, which is what we want: never remove a directory
    // someone else put things in.
    let _ = std::fs::remove_dir(INSTALL_DIR);

    println!("uninstalled.");
    Ok(())
}

/// Create `dir` and verify it ends up owned by root and writable only by root.
fn root_owned_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    chown_root(dir)?;

    let meta = std::fs::metadata(dir)?;
    let mode = meta.permissions().mode();
    // Refuse rather than install a root-executed binary somewhere another user
    // could overwrite it.
    if std::os::unix::fs::MetadataExt::uid(&meta) != 0 || mode & 0o022 != 0 {
        bail!(
            "{} is not root-owned and root-writable-only (uid {}, mode {:o}); refusing to install",
            dir.display(),
            std::os::unix::fs::MetadataExt::uid(&meta),
            mode & 0o777
        );
    }
    Ok(())
}

fn chown_root(path: &Path) -> Result<()> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    if unsafe { libc::chown(c.as_ptr(), 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("setting root ownership on {}", path.display()));
    }
    Ok(())
}

fn write_root_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    chown_root(path)?;
    info!("wrote {}", path.display());
    Ok(())
}

/// launchd rejects a bootstrap while the previous job is still exiting, so give
/// it a moment rather than failing a reinstall.
fn bootstrap() -> Result<()> {
    let mut last = None;
    for attempt in 0..10 {
        match launchctl(&["bootstrap", "system", PLIST_PATH]) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(200 * (attempt + 1)));
            }
        }
    }
    Err(last.expect("at least one attempt"))
}

fn launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("/bin/launchctl")
        .args(args)
        .output()
        .context("running launchctl")?;
    if !output.status.success() {
        bail!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Generated rather than shipped, so the path can never disagree with where the
/// binary actually landed.
fn plist(binary: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>

    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>run</string>
    </array>

    <!-- Resident: the daemon watches for hotplug itself. -->
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>{LOG_PATH}</string>
    <key>StandardErrorPath</key>
    <string>{LOG_PATH}</string>

    <!-- On the packet path: Background throttling stalls the link while the Mac is busy, e.g. after boot. -->
    <key>ProcessType</key>
    <string>Standard</string>
</dict>
</plist>
"#,
        binary.display()
    )
}

/// `B` keeps newsyslog's own "turned over" line out of the log.
const NEWSYSLOG_CONF: &str = "\
# logfilename          [owner:group]  mode count size  when flags [/pid_file]
/var/log/muta.log      root:wheel     644   5     1000  *    JB    /var/run/muta.pid
";

/// Under launchd stdout/stderr are the log file, which newsyslog moves aside;
/// without reopening it, output goes to the rotated file and is lost. Returns
/// whether this process took that on, i.e. runs as the service.
pub fn follow_log_rotation() -> bool {
    if !stderr_is_log() {
        return false;
    }
    signals::reopen_log_on_hangup();
    if let Err(e) = std::fs::write(PID_PATH, format!("{}\n", std::process::id())) {
        warn!("writing {PID_PATH}: {e}; output after the next log rotation will be lost");
    }
    true
}

pub fn release_log() {
    let _ = std::fs::remove_file(PID_PATH);
}

fn stderr_is_log() -> bool {
    let Ok(stderr) = std::io::stderr().as_fd().try_clone_to_owned() else {
        return false;
    };
    match (
        std::fs::File::from(stderr).metadata(),
        std::fs::metadata(LOG_PATH),
    ) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}
