//! Installing and removing the LaunchDaemon.
//!
//! The daemon runs as root, so its binary must live somewhere only root can
//! write. `/usr/local/bin` is group-writable by `admin` on stock macOS, which
//! would let any admin-level process replace the binary and gain root at the
//! next launch — hence the dedicated root-owned directory below.

use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use log::info;

use crate::status::SOCKET_PATH;

pub const LABEL: &str = "dev.jost.muta";
const INSTALL_DIR: &str = "/usr/local/libexec/muta";
const PLIST_PATH: &str = "/Library/LaunchDaemons/dev.jost.muta.plist";
const NEWSYSLOG_PATH: &str = "/etc/newsyslog.d/muta.conf";
pub const LOG_PATH: &str = "/var/log/muta.log";

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

    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#,
        binary.display()
    )
}

/// The daemon logs every 10s forever; without this the log grows without bound.
const NEWSYSLOG_CONF: &str = "\
# logfilename          [owner:group]  mode count size  when flags
/var/log/muta.log      root:wheel     644   5     1000  *    J
";
