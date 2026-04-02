//! GitHub Codespace lifecycle management.
//!
//! Create, reuse, start, stop codespaces and set up SSH config.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::process::Command;
use tracing::{debug, info, warn};

use super::DevEnvConfig;

/// Info returned after attaching a codespace.
pub struct CodespaceInfo {
    pub codespace_name: String,
    pub ssh_host: String,
    pub remote_workdir: String,
}

/// Attach a codespace: find or create, ensure running, set up SSH.
pub fn attach(config: &DevEnvConfig) -> Result<CodespaceInfo> {
    let DevEnvConfig::Codespace { repo, .. } = config;

    let existing = find_existing(repo)?;

    let codespace_name = match existing {
        Some(name) => {
            info!(name = %name, "reusing existing codespace");
            ensure_running(&name)?;
            name
        }
        None => {
            info!(repo = %repo, "creating new codespace");
            create(config)?
        }
    };

    let ssh_host = setup_ssh(&codespace_name)?;

    let repo_name = repo.split('/').last().unwrap_or(repo);
    let remote_workdir = format!("/workspaces/{}", repo_name);

    debug!(
        codespace = %codespace_name,
        ssh_host = %ssh_host,
        remote_workdir = %remote_workdir,
        "codespace attached"
    );

    Ok(CodespaceInfo {
        codespace_name,
        ssh_host,
        remote_workdir,
    })
}

/// Stop a codespace.
pub fn detach(codespace_name: &str) -> Result<()> {
    info!(name = %codespace_name, "stopping codespace");

    let output = Command::new("gh")
        .args(["codespace", "stop", "--codespace", codespace_name])
        .output()
        .context("Failed to execute gh codespace stop")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("already stopped") && !stderr.contains("not found") {
            return Err(anyhow!("Failed to stop codespace: {}", stderr.trim()));
        }
    }

    Ok(())
}

/// Find an existing codespace for a repository.
fn find_existing(repo: &str) -> Result<Option<String>> {
    let output = Command::new("gh")
        .args([
            "codespace", "list", "--repo", repo,
            "--json", "name,state", "--limit", "1",
        ])
        .output()
        .context("Failed to execute gh codespace list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to list codespaces: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let codespaces: Vec<serde_json::Value> =
        serde_json::from_str(&stdout).context("Failed to parse codespace list")?;

    Ok(codespaces
        .first()
        .and_then(|cs| cs.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())))
}

/// Create a new codespace from config.
fn create(config: &DevEnvConfig) -> Result<String> {
    let DevEnvConfig::Codespace {
        repo,
        machine,
        branch,
        location,
        devcontainer_path,
        display_name,
        idle_timeout,
        retention_period,
        ..
    } = config;

    eprintln!("Creating codespace (this may take a minute)...");

    let mut cmd = Command::new("gh");
    cmd.args(["codespace", "create", "--repo", repo]);

    if let Some(m) = machine {
        cmd.args(["--machine", m]);
    }
    if let Some(b) = branch {
        cmd.args(["--branch", b]);
    }
    if let Some(l) = location {
        cmd.args(["--location", l]);
    }
    if let Some(d) = devcontainer_path {
        cmd.args(["--devcontainer-path", d]);
    }
    if let Some(dn) = display_name {
        cmd.args(["--display-name", dn]);
    }
    if let Some(it) = idle_timeout {
        cmd.args(["--idle-timeout", it]);
    }
    if let Some(rp) = retention_period {
        cmd.args(["--retention-period", rp]);
    }

    let output = cmd.output().context("Failed to execute gh codespace create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Permission denied") || stderr.contains("authorize") {
            return Err(anyhow!(
                "Codespace needs permission authorization. Approve in the browser and retry.\n{}",
                stderr.trim()
            ));
        }
        return Err(anyhow!("Failed to create codespace: {}", stderr.trim()));
    }

    // Wait briefly, then find it
    std::thread::sleep(std::time::Duration::from_secs(2));

    let codespace_name = find_existing(repo)?
        .ok_or_else(|| anyhow!("Codespace created but not found in list"))?;

    // Wait for Available state
    eprintln!("Waiting for codespace to be ready...");
    for _ in 0..30 {
        if let Some(state) = get_state(&codespace_name)? {
            if state == "Available" {
                return Ok(codespace_name);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    warn!(name = %codespace_name, "codespace may not be fully ready");
    Ok(codespace_name)
}

/// Ensure a codespace is running.
fn ensure_running(codespace_name: &str) -> Result<()> {
    let state = get_state(codespace_name)?
        .unwrap_or_else(|| "Unknown".to_string());

    if state == "Available" {
        debug!(codespace = %codespace_name, "already running");
        return Ok(());
    }

    info!(codespace = %codespace_name, state = %state, "starting codespace");

    let output = Command::new("gh")
        .args(["codespace", "ssh", "--codespace", codespace_name, "--", "true"])
        .output()
        .context("Failed to start codespace")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to start codespace: {}", stderr.trim()));
    }

    Ok(())
}

/// Get the current state of a codespace.
fn get_state(codespace_name: &str) -> Result<Option<String>> {
    let output = Command::new("gh")
        .args([
            "codespace", "view", "--codespace", codespace_name,
            "--json", "state",
        ])
        .output()
        .context("Failed to check codespace state")?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse codespace state")?;

    Ok(val.get("state").and_then(|s| s.as_str()).map(|s| s.to_string()))
}

/// Set up SSH config for a codespace. Returns the SSH host alias.
fn setup_ssh(codespace_name: &str) -> Result<String> {
    let output = Command::new("gh")
        .args(["codespace", "ssh", "--config", "--codespace", codespace_name])
        .output()
        .context("Failed to get SSH config")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to get SSH config: {}", stderr.trim()));
    }

    let config_entry = String::from_utf8_lossy(&output.stdout);

    let ssh_host = config_entry
        .lines()
        .find(|line| line.starts_with("Host "))
        .and_then(|line| line.strip_prefix("Host "))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("Could not parse SSH host from config"))?;

    // Append to ~/.ssh/config if not already present
    let ssh_config_path = home::home_dir()
        .ok_or_else(|| anyhow!("Could not find home directory"))?
        .join(".ssh/config");

    let existing_config = fs::read_to_string(&ssh_config_path).unwrap_or_default();

    if !existing_config.contains(&format!("Host {}", ssh_host)) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ssh_config_path)
            .context("Failed to open SSH config")?;
        writeln!(file, "\n{}", config_entry.trim())?;
        debug!(ssh_host = %ssh_host, "added SSH config entry");
    }

    Ok(ssh_host)
}

/// Check if SSH to a codespace works.
pub fn check_ssh(ssh_host: &str) -> Result<bool> {
    let output = Command::new("ssh")
        .args(["-o", "ConnectTimeout=5", ssh_host, "true"])
        .output()
        .context("Failed to check SSH connection")?;

    Ok(output.status.success())
}

/// Ensure a codespace is running by name (public wrapper).
pub fn ensure_running_by_name(codespace_name: &str) -> Result<()> {
    ensure_running(codespace_name)
}

/// Set up SSH config for a codespace by name (public wrapper).
pub fn setup_ssh_by_name(codespace_name: &str) -> Result<String> {
    setup_ssh(codespace_name)
}
