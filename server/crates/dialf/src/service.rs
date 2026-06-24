//! Install `dialfd` as an OS-managed background service.
//!
//! macOS → launchd (LaunchDaemon for system scope, LaunchAgent for user scope).
//! Linux → systemd (system unit, or `--user` unit).
//!
//! System scope runs at boot and needs root (run via `sudo`); user scope runs at
//! login and needs no privileges (handy for dev). The generated unit runs
//! `<this-binary> daemon [--config <path>]`.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// launchd label / systemd unit base name.
const LABEL: &str = "build.agora.dialfd";

/// Which privilege/lifecycle scope to install under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Runs at boot, all users; needs root.
    System,
    /// Runs at login for the current user; no root.
    User,
}

/// Service lifecycle actions.
#[derive(Debug, Clone, Copy)]
pub enum Action {
    Install,
    Uninstall,
    Start,
    Stop,
    Status,
}

/// Entry point from the CLI.
pub fn run(action: Action, scope: Scope, config: Option<PathBuf>) -> Result<()> {
    match action {
        Action::Install => install(scope, config),
        Action::Uninstall => uninstall(scope),
        Action::Start => start(scope),
        Action::Stop => stop(scope),
        Action::Status => status(scope),
    }
}

fn exe_path() -> Result<String> {
    let p = std::env::current_exe().context("resolve current executable")?;
    Ok(p.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// Install / uninstall
// ---------------------------------------------------------------------------

fn install(scope: Scope, config: Option<PathBuf>) -> Result<()> {
    let exe = exe_path()?;
    let cfg = config.map(|p| p.to_string_lossy().to_string());
    let path = unit_path(scope);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }

    let contents = if cfg!(target_os = "macos") {
        launchd_plist(&exe, cfg.as_deref(), scope)
    } else {
        systemd_unit(&exe, cfg.as_deref(), scope)
    };

    std::fs::write(&path, contents).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::anyhow!(
                "permission denied writing {} — system scope needs root: re-run with `sudo`, \
                 or use `--user`",
                path.display()
            )
        } else {
            anyhow::anyhow!("write {}: {e}", path.display())
        }
    })?;
    println!("wrote {}", path.display());

    load(scope, &path)?;
    println!("dialfd service installed and started ({:?} scope)", scope);
    Ok(())
}

fn uninstall(scope: Scope) -> Result<()> {
    let _ = unload(scope, &unit_path(scope));
    let path = unit_path(scope);
    match std::fs::remove_file(&path) {
        Ok(_) => println!("removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("not installed ({})", path.display())
        }
        Err(e) => bail!("remove {}: {e}", path.display()),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Platform: unit file locations + contents
// ---------------------------------------------------------------------------

fn unit_path(scope: Scope) -> PathBuf {
    if cfg!(target_os = "macos") {
        match scope {
            Scope::System => PathBuf::from(format!("/Library/LaunchDaemons/{LABEL}.plist")),
            Scope::User => home_dir()
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
        }
    } else {
        match scope {
            Scope::System => PathBuf::from(format!("/etc/systemd/system/{LABEL}.service")),
            Scope::User => home_dir()
                .join(".config/systemd/user")
                .join(format!("{LABEL}.service")),
        }
    }
}

fn launchd_plist(exe: &str, config: Option<&str>, scope: Scope) -> String {
    let mut args = format!("    <string>{exe}</string>\n");
    if let Some(c) = config {
        args.push_str("    <string>--config</string>\n");
        args.push_str(&format!("    <string>{c}</string>\n"));
    }
    args.push_str("    <string>daemon</string>\n");

    // Logs: system scope to /var/log, user scope to the user's Library/Logs.
    let (out, err) = match scope {
        Scope::System => (
            "/var/log/dialfd.out.log".to_string(),
            "/var/log/dialfd.err.log".to_string(),
        ),
        Scope::User => {
            let base = home_dir().join("Library/Logs");
            (
                base.join("dialfd.out.log").to_string_lossy().to_string(),
                base.join("dialfd.err.log").to_string_lossy().to_string(),
            )
        }
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
</dict>
</plist>
"#
    )
}

fn systemd_unit(exe: &str, config: Option<&str>, scope: Scope) -> String {
    let exec = match config {
        Some(c) => format!("{exe} --config {c} daemon"),
        None => format!("{exe} daemon"),
    };
    let install_target = match scope {
        Scope::System => "multi-user.target",
        Scope::User => "default.target",
    };
    format!(
        r#"[Unit]
Description=DialF daemon (dialfd)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={exec}
Restart=always
RestartSec=2
Environment=PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin

[Install]
WantedBy={install_target}
"#
    )
}

// ---------------------------------------------------------------------------
// Platform: load / unload / start / stop / status
// ---------------------------------------------------------------------------

fn load(scope: Scope, path: &std::path::Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        let domain = launchd_domain(scope);
        // Pre-emptive bootout so a reinstall replaces a running instance. Discard its output:
        // when nothing is loaded launchctl prints a misleading "Boot-out failed: 5: Input/
        // output error" (it just means "no such service") — we intentionally ignore it.
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, &path.to_string_lossy()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        run_cmd("launchctl", &["bootstrap", &domain, &path.to_string_lossy()])
            .context("launchctl bootstrap")?;
        let _ = run_cmd("launchctl", &["enable", &format!("{domain}/{LABEL}")]);
    } else {
        systemctl(scope, &["daemon-reload"])?;
        systemctl(scope, &["enable", "--now", &service_name()])?;
    }
    Ok(())
}

fn unload(scope: Scope, path: &std::path::Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        let domain = launchd_domain(scope);
        run_cmd("launchctl", &["bootout", &domain, &path.to_string_lossy()])
    } else {
        let _ = systemctl(scope, &["disable", "--now", &service_name()]);
        systemctl(scope, &["daemon-reload"])
    }
}

fn start(scope: Scope) -> Result<()> {
    if cfg!(target_os = "macos") {
        run_cmd(
            "launchctl",
            &["kickstart", "-k", &format!("{}/{LABEL}", launchd_domain(scope))],
        )
    } else {
        systemctl(scope, &["start", &service_name()])
    }
}

fn stop(scope: Scope) -> Result<()> {
    if cfg!(target_os = "macos") {
        run_cmd(
            "launchctl",
            &["kill", "SIGTERM", &format!("{}/{LABEL}", launchd_domain(scope))],
        )
    } else {
        systemctl(scope, &["stop", &service_name()])
    }
}

fn status(scope: Scope) -> Result<()> {
    if cfg!(target_os = "macos") {
        run_cmd("launchctl", &["print", &format!("{}/{LABEL}", launchd_domain(scope))])
    } else {
        systemctl(scope, &["status", &service_name()])
    }
}

fn launchd_domain(scope: Scope) -> String {
    match scope {
        Scope::System => "system".to_string(),
        Scope::User => format!("gui/{}", libc_getuid()),
    }
}

fn service_name() -> String {
    format!("{LABEL}.service")
}

fn systemctl(scope: Scope, args: &[&str]) -> Result<()> {
    let mut full = Vec::new();
    if scope == Scope::User {
        full.push("--user");
    }
    full.extend_from_slice(args);
    run_cmd("systemctl", &full)
}

fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("spawn `{cmd}`"))?;
    if !status.success() {
        bail!("`{cmd} {}` failed with {status}", args.join(" "));
    }
    Ok(())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// getuid via libc without adding a dependency.
fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}
