//! CLI commands for dev environment management.
//!
//! ```
//! workmux group dev-env attach [--codespace <name>] [--repo <owner/repo>] [--ports 3000,8080]
//! workmux group dev-env detach
//! workmux group dev-env status
//! workmux group dev-env ports [--json]
//! ```

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dev_env::{self, DevEnvConfig, DevEnvState, PortMapping, SyncStrategy};
use crate::dev_env::codespace;
use crate::dev_env::ports::PortPool;
use crate::dev_env::watcher;
use crate::workflow::group::{self, GroupState};

#[derive(Args)]
pub struct DevEnvArgs {
    #[command(subcommand)]
    pub command: DevEnvCommand,
}

#[derive(Subcommand)]
pub enum DevEnvCommand {
    /// Attach a dev environment to the current group workspace
    Attach {
        /// Attach to an existing codespace by name
        #[arg(long)]
        codespace: Option<String>,

        /// Repository for a new codespace (e.g., acme-corp/webapp)
        #[arg(long)]
        repo: Option<String>,

        /// Machine type for the codespace
        #[arg(long)]
        machine: Option<String>,

        /// Remote ports to forward (comma-separated)
        #[arg(long, value_delimiter = ',')]
        ports: Option<Vec<u16>>,

        /// Path to devcontainer.json
        #[arg(long)]
        devcontainer_path: Option<String>,
    },

    /// Detach the dev environment from the current group workspace
    Detach,

    /// Show dev environment status (codespace + tunnels)
    Status,

    /// Show the port map
    Ports {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

/// Resolve group state from cwd.
fn resolve_group_state() -> Result<(GroupState, std::path::PathBuf)> {
    let cwd = std::env::current_dir()?;
    let ws_dir = find_group_workspace(&cwd)?;
    let state = GroupState::load(&ws_dir)?;
    Ok((state, ws_dir))
}

/// Walk up from cwd to find a group workspace directory.
fn find_group_workspace(start: &std::path::Path) -> Result<std::path::PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(group::STATE_FILE).exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    // Also check if cwd IS the workspace (symlinks resolved)
    let canonical = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    if canonical.join(group::STATE_FILE).exists() {
        return Ok(canonical);
    }
    bail!("Not in a workmux group workspace. Run from a group workspace directory.")
}

pub fn run(args: DevEnvArgs) -> Result<()> {
    match args.command {
        DevEnvCommand::Attach {
            codespace: cs_name,
            repo,
            machine,
            ports,
            devcontainer_path,
        } => run_attach(cs_name, repo, machine, ports, devcontainer_path),
        DevEnvCommand::Detach => run_detach(),
        DevEnvCommand::Status => run_status(),
        DevEnvCommand::Ports { json } => run_ports(json),
    }
}

fn run_attach(
    cs_name: Option<String>,
    repo: Option<String>,
    machine: Option<String>,
    ports: Option<Vec<u16>>,
    devcontainer_path: Option<String>,
) -> Result<()> {
    let (mut state, ws_dir) = resolve_group_state()?;

    if state.dev_env.as_ref().is_some_and(|d| d.is_attached()) {
        bail!("Dev environment already attached. Run 'workmux group dev-env detach' first.");
    }

    // Build config: from explicit args, or fall back to group config
    let config = if let Some(repo) = repo {
        DevEnvConfig::Codespace {
            repo,
            machine,
            auto_attach: false,
            branch: None,
            location: None,
            devcontainer_path,
            display_name: None,
            idle_timeout: None,
            retention_period: None,
            ports: ports.unwrap_or_default(),
            sync: SyncStrategy::GitPush,
            devloop: None,
        }
    } else if let Some(ref existing) = state.dev_env {
        // Re-use config from state (was set during group add from config.yaml)
        existing.config.clone()
    } else {
        bail!(
            "No dev environment configured. Specify --repo <owner/repo> or add dev_env to group config."
        );
    };

    // If attaching to existing codespace by name, skip create
    let cs_info = if let Some(name) = cs_name {
        println!("Attaching to existing codespace {}...", name);
        codespace::ensure_running_by_name(&name)?;
        let ssh_host = codespace::setup_ssh_by_name(&name)?;
        let repo_str = match &config {
            DevEnvConfig::Codespace { repo, .. } => repo.clone(),
        };
        let repo_name = repo_str.split('/').last().unwrap_or(&repo_str);
        codespace::CodespaceInfo {
            codespace_name: name,
            ssh_host,
            remote_workdir: format!("/workspaces/{}", repo_name),
        }
    } else {
        codespace::attach(&config)?
    };

    // Allocate ports
    let remote_ports = config.ports();
    let port_mappings = if !remote_ports.is_empty() {
        let mut pool = PortPool::load()?;
        let locals = pool.allocate(&state.group_name, &state.branch, remote_ports.len())?;
        pool.save()?;
        remote_ports
            .iter()
            .zip(locals.iter())
            .map(|(&remote, &local)| PortMapping { remote, local })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    // Spawn watcher if we have ports to forward
    let watcher_pid = if !port_mappings.is_empty() {
        Some(spawn_watcher(
            &cs_info.ssh_host,
            &port_mappings,
            &state.group_name,
            &state.branch,
        )?)
    } else {
        None
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Update state
    state.dev_env = Some(DevEnvState {
        config,
        codespace_name: Some(cs_info.codespace_name.clone()),
        ssh_host: Some(cs_info.ssh_host.clone()),
        remote_workdir: Some(cs_info.remote_workdir.clone()),
        attached_at: Some(now),
        port_mappings: port_mappings.clone(),
        watcher_pid,
    });

    state.save(&ws_dir)?;

    println!();
    println!("✓ Dev environment attached:");
    println!("  Codespace: {}", cs_info.codespace_name);
    println!("  SSH host:  {}", cs_info.ssh_host);
    println!("  Remote:    {}", cs_info.remote_workdir);

    if !port_mappings.is_empty() {
        println!("  Ports:");
        for pm in &port_mappings {
            println!("    {} → localhost:{}", pm.remote, pm.local);
        }
        if let Some(pid) = watcher_pid {
            println!("  Watcher:   PID {}", pid);
        }
    }

    Ok(())
}

fn run_detach() -> Result<()> {
    let (mut state, ws_dir) = resolve_group_state()?;

    let dev_env = state
        .dev_env
        .as_ref()
        .ok_or_else(|| anyhow!("No dev environment attached to this group"))?;

    if !dev_env.is_attached() {
        bail!("No dev environment attached to this group");
    }

    println!("Detaching dev environment...");

    // Kill watcher
    watcher::kill_watcher(&state.group_name, &state.branch)?;

    // Release ports
    let mut pool = PortPool::load()?;
    pool.release(&state.group_name, &state.branch);
    pool.save()?;

    // Stop codespace
    if let Some(name) = dev_env.codespace_name() {
        codespace::detach(name)?;
    }

    // Reset to unattached config state (preserves config for re-attach)
    state.dev_env = Some(DevEnvState::from_config(dev_env.config.clone()));
    state.save(&ws_dir)?;

    println!("✓ Dev environment detached");

    Ok(())
}

fn run_status() -> Result<()> {
    let (state, _) = resolve_group_state()?;

    let dev_env = match &state.dev_env {
        Some(d) if d.is_attached() => d,
        _ => {
            println!("No dev environment attached.");
            return Ok(());
        }
    };

    let DevEnvConfig::Codespace { repo, machine, sync, devloop, .. } = &dev_env.config;

    println!("Dev Environment: codespace ({})", repo);
    if let Some(m) = machine {
        println!("  Machine:    {}", m);
    }
    if let Some(name) = &dev_env.codespace_name {
        println!("  Codespace:  {}", name);
    }
    if let Some(host) = &dev_env.ssh_host {
        println!("  SSH host:   {}", host);
    }
    if let Some(dir) = &dev_env.remote_workdir {
        println!("  Remote dir: {}", dir);
    }
    println!("  Sync:       {:?}", sync);

    // Watcher status
    let watcher_alive = watcher::read_watcher_pid(&state.group_name, &state.branch);
    match (dev_env.watcher_pid, watcher_alive) {
        (Some(pid), Some(_)) => println!("  Watcher:    PID {} (running)", pid),
        (Some(pid), None) => println!("  Watcher:    PID {} (dead)", pid),
        _ => {}
    }

    // Tunnel status
    if !dev_env.port_mappings.is_empty() {
        println!();
        println!("  Tunnels:");
        // Read recent log events to determine per-tunnel status
        let events = watcher::read_recent_log(&state.group_name, &state.branch, 50);
        for pm in &dev_env.port_mappings {
            let tunnel_key = format!("{}→{}", pm.remote, pm.local);
            let last_event = events.iter().find(|e| {
                e.get("tunnel")
                    .and_then(|t| t.as_str())
                    .map(|t| t == tunnel_key)
                    .unwrap_or(false)
            });
            let status = last_event
                .and_then(|e| e.get("event").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            let icon = match status {
                "started" => "✓",
                "reconnecting" => "↻",
                "died" | "spawn_failed" => "✗",
                _ => "?",
            };
            println!("    {} → localhost:{}  {} {}", pm.remote, pm.local, icon, status);
        }
    }

    if let Some(dl) = devloop {
        println!();
        println!("  Devloop:");
        for line in dl.lines() {
            println!("    {}", line);
        }
    }

    Ok(())
}

fn run_ports(json: bool) -> Result<()> {
    let (state, _) = resolve_group_state()?;

    let dev_env = match &state.dev_env {
        Some(d) if d.is_attached() => d,
        _ => {
            if json {
                println!("[]");
            } else {
                println!("No dev environment attached.");
            }
            return Ok(());
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&dev_env.port_mappings)?);
    } else if dev_env.port_mappings.is_empty() {
        println!("No ports forwarded.");
    } else {
        println!("Port map:");
        for pm in &dev_env.port_mappings {
            println!("  {} → localhost:{}", pm.remote, pm.local);
        }
    }

    Ok(())
}

/// Spawn the watcher as a daemonized background process.
/// Returns the watcher's PID.
fn spawn_watcher(
    ssh_host: &str,
    port_mappings: &[PortMapping],
    group_name: &str,
    branch: &str,
) -> Result<u32> {
    // Serialize args to pass to the watcher subprocess
    let mappings_json = serde_json::to_string(port_mappings)?;

    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("workmux"));

    let child = std::process::Command::new(&exe)
        .args([
            "_dev-env-watcher",
            "--ssh-host", ssh_host,
            "--mappings", &mappings_json,
            "--group", group_name,
            "--branch", branch,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn watcher: {}", e))?;

    let pid = child.id();
    Ok(pid)
}

/// Attach dev env during group add (called from workflow::group).
/// Uses config from GroupConfig if present.
pub fn auto_attach(
    group_config: &crate::config::GroupConfig,
    state: &mut GroupState,
    ws_dir: &std::path::Path,
) -> Result<()> {
    let config = match &group_config.dev_env {
        Some(c) => c.clone(),
        None => return Ok(()), // No dev_env in config, nothing to do
    };

    if !config.auto_attach() {
        return Ok(()); // dev_env present but auto-attach not requested
    }

    println!("Attaching dev environment...");

    let cs_info = codespace::attach(&config)?;

    let remote_ports = config.ports();
    let port_mappings = if !remote_ports.is_empty() {
        let mut pool = PortPool::load()?;
        let locals = pool.allocate(&state.group_name, &state.branch, remote_ports.len())?;
        pool.save()?;
        remote_ports
            .iter()
            .zip(locals.iter())
            .map(|(&remote, &local)| PortMapping { remote, local })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let watcher_pid = if !port_mappings.is_empty() {
        Some(spawn_watcher(
            &cs_info.ssh_host,
            &port_mappings,
            &state.group_name,
            &state.branch,
        )?)
    } else {
        None
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    state.dev_env = Some(DevEnvState {
        config,
        codespace_name: Some(cs_info.codespace_name.clone()),
        ssh_host: Some(cs_info.ssh_host.clone()),
        remote_workdir: Some(cs_info.remote_workdir.clone()),
        attached_at: Some(now),
        port_mappings,
        watcher_pid,
    });

    state.save(ws_dir)?;

    println!("✓ Dev environment attached: {}", cs_info.codespace_name);

    Ok(())
}

/// Detach dev env during group remove.
pub fn auto_detach(state: &GroupState) -> Result<()> {
    let dev_env = match &state.dev_env {
        Some(d) if d.is_attached() => d,
        _ => return Ok(()),
    };

    println!("Detaching dev environment...");

    // Kill watcher
    watcher::kill_watcher(&state.group_name, &state.branch)?;

    // Release ports
    let mut pool = PortPool::load()?;
    pool.release(&state.group_name, &state.branch);
    pool.save()?;

    // Stop codespace
    if let Some(name) = dev_env.codespace_name() {
        codespace::detach(name)?;
    }

    Ok(())
}
