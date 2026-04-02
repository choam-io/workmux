//! Background watcher process for managing SSH tunnels.
//!
//! Spawned by the CLI, babysits per-port SSH tunnel child processes.
//! Reconnects on failure with exponential backoff.
//! Logs structured JSON lines to a log file for observability.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tracing::{debug, info};

use super::tunnel::Tunnel;
use super::PortMapping;

const MIN_BACKOFF_MS: u64 = 2000;
const MAX_BACKOFF_MS: u64 = 30000;
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// A JSON line event written to the watcher log.
#[derive(Debug, Serialize)]
struct LogEvent {
    ts: String,
    tunnel: String,
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backoff_ms: Option<u64>,
}

impl LogEvent {
    fn new(mapping: &PortMapping, event: &'static str) -> Self {
        Self {
            ts: now_iso(),
            tunnel: format!("{}→{}", mapping.remote, mapping.local),
            event,
            pid: None,
            exit_code: None,
            attempt: None,
            backoff_ms: None,
        }
    }
}

fn now_iso() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

/// State for one managed tunnel with reconnect tracking.
struct ManagedTunnel {
    mapping: PortMapping,
    tunnel: Option<Tunnel>,
    backoff_ms: u64,
    attempt: u32,
}

/// Get the watcher log directory for a group.
pub fn watcher_dir(group_name: &str, branch: &str) -> Result<PathBuf> {
    let home = home::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let slug = slug::slugify(branch);
    let dir = home
        .join(".local/share/workmux/watcher")
        .join(format!("{}--{}", group_name, slug));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Run the watcher loop. This function blocks and manages tunnels until killed.
///
/// Intended to be called from a daemonized child process.
pub fn run(
    ssh_host: &str,
    mappings: &[PortMapping],
    group_name: &str,
    branch: &str,
) -> Result<()> {
    let dir = watcher_dir(group_name, branch)?;
    let log_path = dir.join("watcher.log");
    let pid_path = dir.join("watcher.pid");

    // Write our PID
    fs::write(&pid_path, std::process::id().to_string())?;

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("Failed to open watcher log")?;

    info!(
        group = group_name,
        branch = branch,
        tunnels = mappings.len(),
        "watcher:starting"
    );

    // Spawn initial tunnels
    let mut managed: Vec<ManagedTunnel> = mappings
        .iter()
        .map(|m| {
            let tunnel = spawn_and_log(ssh_host, m, &mut log_file);
            ManagedTunnel {
                mapping: m.clone(),
                tunnel,
                backoff_ms: MIN_BACKOFF_MS,
                attempt: 0,
            }
        })
        .collect();

    // Health check loop
    loop {
        std::thread::sleep(HEALTH_CHECK_INTERVAL);

        for mt in &mut managed {
            let alive = mt
                .tunnel
                .as_mut()
                .map(|t| t.is_alive())
                .unwrap_or(false);

            if !alive {
                // Log death if we had a tunnel
                if let Some(ref mut t) = mt.tunnel {
                    let exit_code = t.child.try_wait().ok().flatten().and_then(|s| s.code());
                    let mut ev = LogEvent::new(&mt.mapping, "died");
                    ev.exit_code = exit_code;
                    log_event(&mut log_file, &ev);
                    mt.tunnel = None;
                }

                // Reconnect with backoff
                mt.attempt += 1;
                let mut ev = LogEvent::new(&mt.mapping, "reconnecting");
                ev.attempt = Some(mt.attempt);
                ev.backoff_ms = Some(mt.backoff_ms);
                log_event(&mut log_file, &ev);

                std::thread::sleep(Duration::from_millis(mt.backoff_ms));

                mt.tunnel = spawn_and_log(ssh_host, &mt.mapping, &mut log_file);

                if mt.tunnel.is_some() {
                    // Success: reset backoff
                    mt.backoff_ms = MIN_BACKOFF_MS;
                    mt.attempt = 0;
                } else {
                    // Failure: increase backoff
                    mt.backoff_ms = (mt.backoff_ms * 2).min(MAX_BACKOFF_MS);
                }
            }
        }
    }
}

/// Spawn a tunnel and log the result.
fn spawn_and_log(
    ssh_host: &str,
    mapping: &PortMapping,
    log_file: &mut File,
) -> Option<Tunnel> {
    match Tunnel::spawn(ssh_host, mapping.local, mapping.remote) {
        Ok(tunnel) => {
            let mut ev = LogEvent::new(mapping, "started");
            ev.pid = Some(tunnel.pid());
            log_event(log_file, &ev);
            Some(tunnel)
        }
        Err(e) => {
            debug!(error = %e, "tunnel:spawn failed");
            let ev = LogEvent::new(mapping, "spawn_failed");
            log_event(log_file, &ev);
            None
        }
    }
}

fn log_event(file: &mut File, event: &LogEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        let _ = writeln!(file, "{}", json);
        let _ = file.flush();
    }
}

/// Read the watcher PID from disk. Returns None if not running.
pub fn read_watcher_pid(group_name: &str, branch: &str) -> Option<u32> {
    let dir = watcher_dir(group_name, branch).ok()?;
    let pid_str = fs::read_to_string(dir.join("watcher.pid")).ok()?;
    let pid: u32 = pid_str.trim().parse().ok()?;

    // Check if process is actually alive
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            Some(pid)
        } else {
            None
        }
    }
}

/// Kill the watcher process for a group.
pub fn kill_watcher(group_name: &str, branch: &str) -> Result<()> {
    if let Some(pid) = read_watcher_pid(group_name, branch) {
        info!(pid, group = group_name, branch = branch, "killing watcher");
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        // Clean up PID file
        if let Ok(dir) = watcher_dir(group_name, branch) {
            let _ = fs::remove_file(dir.join("watcher.pid"));
        }
    }
    Ok(())
}

/// Read recent log events for status display.
pub fn read_recent_log(group_name: &str, branch: &str, count: usize) -> Vec<serde_json::Value> {
    let dir = match watcher_dir(group_name, branch) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let log_path = dir.join("watcher.log");
    let content = match fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .rev()
        .take(count)
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
