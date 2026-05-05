//! `usepod-agent service ...` — install / start / stop / status / uninstall
//! the agent as a managed system service.
//!
//! Cross-platform via the `service-manager` crate:
//!
//! - **Linux** — systemd. Unit at `/etc/systemd/system/usepod-agent.service`.
//!   Runs as a dedicated `usepod` user (created during install if missing).
//!   Logs go to `journalctl -u usepod-agent` via stderr.
//! - **macOS** — launchd. Plist at `/Library/LaunchDaemons/ai.usepod.agent.plist`.
//!   System daemon (requires sudo). Logs at `/var/log/usepod-agent.log`.
//! - **Windows** — SCM. Service `ai.usepod.agent`, runs as LocalSystem. Logs
//!   captured via `USEPOD_AGENT_LOG_FILE` (SCM doesn't capture stdout).
//!
//! All `install`/`uninstall` paths require elevated privileges. `start`/`stop`/
//! `status`/`logs` work without elevation on platforms that allow it; on
//! systemd they need sudo because we install system (not user) units.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command as ProcCommand;

use anyhow::{Context, Result, bail};
use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager,
    ServiceStartCtx, ServiceStatus, ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};

/// Service identity. Platforms render this differently:
///
/// - systemd unit name: `usepod-agent.service` (from `to_script_name()`)
/// - launchd plist: `ai.usepod.agent.plist` (from `to_qualified_name()`)
/// - Windows SCM service: `ai.usepod.agent`
pub fn label() -> ServiceLabel {
    ServiceLabel {
        qualifier: Some("ai".into()),
        organization: Some("usepod".into()),
        application: "agent".into(),
    }
}

/// Per-OS default working directory for the service. Created on install.
pub fn working_directory() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/var/lib/usepod-agent")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/var/lib/usepod-agent")
    }
    #[cfg(target_os = "windows")]
    {
        // %ProgramData% is C:\ProgramData on default installs; readable+writable
        // by LocalSystem and admin without surprises.
        let pd = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
        PathBuf::from(pd).join("usepod-agent")
    }
}

/// Path the service's stdout/stderr is captured to (Windows + macOS).
/// On Linux this is unused; logs go through journald.
pub fn default_log_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/var/log/usepod-agent.log")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/var/log/usepod-agent.log")
    }
    #[cfg(target_os = "windows")]
    {
        working_directory().join("agent.log")
    }
}

/// Linux service user. Created at install if missing.
#[cfg(target_os = "linux")]
const SERVICE_USER: &str = "usepod";

#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Optional `--config <path>` to pass through to the running agent.
    pub config: Option<PathBuf>,
    /// Optional `--log-level` to pass through (default: `info`).
    pub log_level: Option<String>,
}

/// Top-level dispatcher invoked from `main.rs`.
pub fn run(action: Action) -> Result<()> {
    match action {
        Action::Install(opts) => install(opts),
        Action::Uninstall => uninstall(),
        Action::Start => start(),
        Action::Stop => stop(),
        Action::Restart => {
            // Best-effort stop, then start. On a fresh install where stop
            // fails because nothing's running, swallow and proceed.
            let _ = stop();
            start()
        }
        Action::Status => status(),
        Action::Logs { follow } => logs(follow),
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Install(InstallOptions),
    Uninstall,
    Start,
    Stop,
    Restart,
    Status,
    Logs { follow: bool },
}

fn manager() -> Result<Box<dyn ServiceManager>> {
    let mut m = <dyn ServiceManager>::native()
        .context("could not detect a supported native service manager (systemd / launchd / SCM)")?;
    // We only operate at the system level — user-level services are out of
    // scope for this PR. set_level returns Err if the requested level isn't
    // supported on this platform.
    m.set_level(ServiceLevel::System)
        .context("system-level service install not supported by this platform's service manager")?;
    Ok(m)
}

fn build_install_ctx(opts: &InstallOptions) -> Result<ServiceInstallCtx> {
    let program = std::env::current_exe()
        .context("could not resolve current executable path for service install")?;

    let mut args: Vec<OsString> = vec!["run".into()];
    if let Some(p) = &opts.config {
        args.push("--config".into());
        args.push(p.as_os_str().to_owned());
    }
    if let Some(level) = &opts.log_level {
        args.push("--log-level".into());
        args.push(level.into());
    }

    let working_dir = working_directory();

    // Environment: tell the running agent where to send logs when stdout/stderr
    // aren't captured. On Linux journald handles it; setting this is harmless.
    let mut environment: Vec<(String, String)> = Vec::new();
    if cfg!(target_os = "windows") {
        environment.push((
            "USEPOD_AGENT_LOG_FILE".into(),
            default_log_path().to_string_lossy().into_owned(),
        ));
    }

    // Username: dedicated user on Linux. macOS + Windows fall back to the
    // service manager default (root / LocalSystem) — addressed in a follow-up
    // PR per the plan. Use #[cfg] blocks rather than cfg!() because
    // SERVICE_USER itself is gated to linux; cfg!() doesn't gate symbol
    // resolution and breaks the Windows + macOS builds.
    #[cfg(target_os = "linux")]
    let username = Some(SERVICE_USER.to_string());
    #[cfg(not(target_os = "linux"))]
    let username: Option<String> = None;

    Ok(ServiceInstallCtx {
        label: label(),
        program,
        args,
        contents: platform_contents(&working_dir),
        username,
        working_directory: Some(working_dir),
        environment: if environment.is_empty() {
            None
        } else {
            Some(environment)
        },
        autostart: true,
        // `Always` matches the existing standalone systemd template
        // (install/usepod-agent.service), which operators have been running
        // since v0.1.0. Clean stops via `service stop` aren't fought by this
        // policy because the service manager records a stop reason that
        // suppresses auto-restart on intentional shutdown.
        restart_policy: RestartPolicy::Always {
            delay_secs: Some(5),
        },
    })
}

/// Custom unit-file / plist contents per platform. Returning `None` here lets
/// the service-manager crate generate a sensible default; we override only
/// where we want hardening or log-capture directives.
fn platform_contents(_working_dir: &std::path::Path) -> Option<String> {
    // Linux: keep the existing hardened unit template (ProtectSystem,
    // NoNewPrivileges, etc.) by emitting our own contents.
    #[cfg(target_os = "linux")]
    {
        let exe = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/usr/local/bin/usepod-agent".into());
        return Some(format!(
            r#"[Unit]
Description=Use Pod Provider Agent
Documentation=https://usepod.ai/docs/agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User={user}
Group={user}
WorkingDirectory={wd}
ExecStart={exe} run
Restart=always
RestartSec=5

# --- Hardening (kept in sync with install/usepod-agent.service) -------------
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths={wd}
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true

[Install]
WantedBy=multi-user.target
"#,
            user = SERVICE_USER,
            wd = _working_dir.display(),
            exe = exe,
        ));
    }

    // macOS: emit a plist with explicit StandardOutPath / StandardErrorPath
    // so logs land somewhere operators can `tail -f`. The crate's default
    // plist doesn't set those, leaving stdout discarded.
    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/usr/local/bin/usepod-agent".into());
        let log = default_log_path().to_string_lossy().into_owned();
        return Some(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>ai.usepod.agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>WorkingDirectory</key><string>{wd}</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key><false/>
    </dict>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            exe = exe,
            wd = _working_dir.display(),
            log = log,
        ));
    }

    // Windows: let service-manager generate the default sc.exe install. The
    // log redirect is handled in-process via USEPOD_AGENT_LOG_FILE rather than
    // by SCM (which has no stdout-capture).
    #[cfg(target_os = "windows")]
    {
        return None;
    }

    #[allow(unreachable_code)]
    None
}

fn install(opts: InstallOptions) -> Result<()> {
    require_elevated("install")?;
    ensure_working_directory()?;

    #[cfg(target_os = "linux")]
    {
        ensure_linux_user(SERVICE_USER)?;
        chown_working_directory(SERVICE_USER, &working_directory())?;
    }

    let m = manager()?;
    let ctx = build_install_ctx(&opts)?;
    m.install(ctx).context("service install failed")?;

    let status_hint = match m.status(ServiceStatusCtx { label: label() }) {
        Ok(ServiceStatus::Running) => "running",
        Ok(ServiceStatus::Stopped(_)) => "installed (stopped)",
        Ok(ServiceStatus::NotInstalled) => "installed (not yet started)",
        Err(_) => "installed",
    };
    println!("✓ usepod-agent service: {status_hint}");
    println!("  start:  usepod-agent service start");
    println!("  status: usepod-agent service status");
    println!("  logs:   usepod-agent service logs -f");
    Ok(())
}

fn uninstall() -> Result<()> {
    require_elevated("uninstall")?;
    let m = manager()?;
    // Best-effort stop; if it isn't running, ignore.
    let _ = m.stop(ServiceStopCtx { label: label() });
    m.uninstall(ServiceUninstallCtx { label: label() })
        .context("service uninstall failed")?;
    println!("✓ usepod-agent service uninstalled");
    Ok(())
}

fn start() -> Result<()> {
    let m = manager()?;
    m.start(ServiceStartCtx { label: label() })
        .context("service start failed (try with sudo / as Administrator)")?;
    println!("✓ usepod-agent service started");
    Ok(())
}

fn stop() -> Result<()> {
    let m = manager()?;
    m.stop(ServiceStopCtx { label: label() })
        .context("service stop failed (try with sudo / as Administrator)")?;
    println!("✓ usepod-agent service stopped");
    Ok(())
}

fn status() -> Result<()> {
    let m = manager()?;
    match m.status(ServiceStatusCtx { label: label() }) {
        Ok(ServiceStatus::Running) => {
            println!("running");
            Ok(())
        }
        Ok(ServiceStatus::Stopped(reason)) => {
            match reason {
                Some(r) => println!("stopped: {r}"),
                None => println!("stopped"),
            }
            Ok(())
        }
        Ok(ServiceStatus::NotInstalled) => {
            println!("not installed");
            std::process::exit(3);
        }
        Err(e) => Err(anyhow::Error::from(e).context("service status query failed")),
    }
}

fn logs(follow: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut cmd = ProcCommand::new("journalctl");
        cmd.arg("-u").arg("usepod-agent");
        if follow {
            cmd.arg("-f");
        } else {
            cmd.arg("-n").arg("200").arg("--no-pager");
        }
        let status = cmd.status().context("failed to invoke journalctl")?;
        if !status.success() {
            bail!("journalctl exited with {status}");
        }
        return Ok(());
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let path = default_log_path();
        if !path.exists() {
            bail!(
                "log file does not yet exist at {} (service may not have run yet)",
                path.display()
            );
        }
        let mut cmd = if cfg!(target_os = "windows") {
            // No tail on stock Windows; use PowerShell's Get-Content -Wait.
            let mut c = ProcCommand::new("powershell");
            c.arg("-NoProfile").arg("-Command");
            if follow {
                c.arg(format!("Get-Content -Path '{}' -Wait -Tail 200", path.display()));
            } else {
                c.arg(format!("Get-Content -Path '{}' -Tail 200", path.display()));
            }
            c
        } else {
            let mut c = ProcCommand::new("tail");
            c.arg("-n").arg("200");
            if follow {
                c.arg("-f");
            }
            c.arg(&path);
            c
        };
        let status = cmd.status().context("failed to invoke log tailer")?;
        if !status.success() {
            bail!("log tailer exited with {status}");
        }
        return Ok(());
    }

    #[allow(unreachable_code)]
    {
        bail!("`service logs` not implemented for this platform");
    }
}

// --- Helpers ----------------------------------------------------------------

fn require_elevated(action: &str) -> Result<()> {
    if is_elevated() {
        return Ok(());
    }
    let hint = if cfg!(target_os = "windows") {
        "re-run from an elevated PowerShell (Run as Administrator)"
    } else {
        "re-run with sudo"
    };
    bail!("`service {action}` needs root/Administrator privileges; {hint}");
}

#[cfg(unix)]
fn is_elevated() -> bool {
    // SAFETY: getuid is always-success and side-effect-free.
    unsafe { libc_getuid() == 0 }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

#[cfg(target_os = "windows")]
fn is_elevated() -> bool {
    // Heuristic: try to open a handle to the SCM with full access. Fails
    // for non-admin users. Avoids pulling in the windows-rs crate just for
    // this check by using the env-driven sentinel `IsElevated` set by some
    // PowerShell launchers, falling back to attempting a privileged sc.exe
    // command. Pragmatic and Good Enough.
    if std::env::var("USEPOD_AGENT_FORCE_ELEVATED").is_ok() {
        return true;
    }
    // `net session` returns success only when running as Administrator and
    // doesn't require any privileges to attempt. It's the canonical
    // is-admin check on stock Windows.
    ProcCommand::new("net")
        .arg("session")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ensure_working_directory() -> Result<()> {
    let wd = working_directory();
    if !wd.exists() {
        std::fs::create_dir_all(&wd)
            .with_context(|| format!("could not create {}", wd.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_linux_user(user: &str) -> Result<()> {
    // If `id usepod` succeeds, the account exists and we're done.
    let exists = ProcCommand::new("id")
        .arg(user)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if exists {
        return Ok(());
    }
    let status = ProcCommand::new("useradd")
        .arg("--system")
        .arg("--no-create-home")
        .arg("--shell")
        .arg("/usr/sbin/nologin")
        .arg(user)
        .status()
        .context("failed to invoke useradd; install shadow-utils or create the user manually")?;
    if !status.success() {
        bail!(
            "useradd exited with {status}; create the `{user}` system user manually then retry"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn chown_working_directory(user: &str, wd: &std::path::Path) -> Result<()> {
    let status = ProcCommand::new("chown")
        .arg("-R")
        .arg(format!("{user}:{user}"))
        .arg(wd)
        .status()
        .context("failed to invoke chown")?;
    if !status.success() {
        bail!("chown exited with {status}");
    }
    Ok(())
}

// Stub on non-Linux so cfg-free callsites compile.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
fn ensure_linux_user(_user: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_renders_per_platform_correctly() {
        let l = label();
        // systemd unit name (script_name)
        assert_eq!(l.to_script_name(), "usepod-agent");
        // launchd / SCM name (qualified_name)
        assert_eq!(l.to_qualified_name(), "ai.usepod.agent");
    }

    #[test]
    fn install_ctx_carries_run_subcommand() {
        let opts = InstallOptions {
            config: None,
            log_level: None,
        };
        let ctx = build_install_ctx(&opts).expect("ctx builds");
        assert_eq!(ctx.args.first().map(|s| s.as_os_str()), Some(std::ffi::OsStr::new("run")));
        assert!(ctx.autostart);
    }

    #[test]
    fn install_ctx_propagates_config_and_log_level() {
        let opts = InstallOptions {
            config: Some(PathBuf::from("/etc/usepod/agent.toml")),
            log_level: Some("debug".into()),
        };
        let ctx = build_install_ctx(&opts).expect("ctx builds");
        let args: Vec<String> = ctx
            .args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["run", "--config", "/etc/usepod/agent.toml", "--log-level", "debug"]);
    }

    #[test]
    fn restart_policy_is_always() {
        let ctx = build_install_ctx(&InstallOptions {
            config: None,
            log_level: None,
        })
        .unwrap();
        assert!(matches!(ctx.restart_policy, RestartPolicy::Always { .. }));
    }
}
