//! GitHub Codespace operations for execution targets.
//!
//! Manages codespace lifecycle: create, start, stop, SSH setup.

use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::Command;
use tracing::{debug, info};

/// Create or reuse a codespace for a repository.
///
/// Returns (codespace_name, ssh_host).
pub fn attach_codespace(repo: &str, _worktree_path: &Path) -> Result<(String, String)> {
    // Check for existing codespace for this repo
    let existing = find_existing_codespace(repo)?;

    let codespace_name = match existing {
        Some(name) => {
            info!(name = %name, "reusing existing codespace");
            ensure_codespace_running(&name)?;
            name
        }
        None => {
            info!(repo = %repo, "creating new codespace");
            create_codespace(repo)?
        }
    };

    // Set up SSH config entry
    let ssh_host = setup_ssh(&codespace_name)?;

    // Determine remote working directory
    let repo_name = repo.split('/').last().unwrap_or(repo);
    let remote_workdir = format!("/workspaces/{}", repo_name);

    debug!(
        codespace = %codespace_name,
        ssh_host = %ssh_host,
        remote_workdir = %remote_workdir,
        "codespace attached"
    );

    Ok((codespace_name, ssh_host))
}

/// Detach from a codespace (stop it).
pub fn detach_codespace(codespace_name: &str) -> Result<()> {
    info!(name = %codespace_name, "stopping codespace");

    let output = Command::new("gh")
        .args(["codespace", "stop", "--codespace", codespace_name])
        .output()
        .context("Failed to execute gh codespace stop")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Don't fail if codespace is already stopped
        if !stderr.contains("already stopped") && !stderr.contains("not found") {
            return Err(anyhow!(
                "Failed to stop codespace: {}",
                stderr.trim()
            ));
        }
    }

    Ok(())
}

/// Find an existing codespace for a repository.
fn find_existing_codespace(repo: &str) -> Result<Option<String>> {
    let output = Command::new("gh")
        .args([
            "codespace",
            "list",
            "--repo",
            repo,
            "--json",
            "name,state",
            "--limit",
            "1",
        ])
        .output()
        .context("Failed to execute gh codespace list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to list codespaces: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let codespaces: Vec<serde_json::Value> = serde_json::from_str(&stdout)
        .context("Failed to parse codespace list")?;

    Ok(codespaces.first().and_then(|cs| {
        cs.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())
    }))
}

/// Create a new codespace for a repository.
fn create_codespace(repo: &str) -> Result<String> {
    // Create codespace without --status (it tries to SSH which can fail)
    // Use default machine type to avoid interactive prompts
    eprintln!("Creating codespace (this may take a minute)...");

    let output = Command::new("gh")
        .args([
            "codespace",
            "create",
            "--repo",
            repo,
            "--machine", "basicLinux32gb",
        ])
        .output()
        .context("Failed to execute gh codespace create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Check if it's just a status polling error but codespace was created
        if !stderr.contains("Permission denied") {
            return Err(anyhow!("Failed to create codespace: {}", stderr.trim()));
        }
    }

    // Wait a moment for codespace to be ready
    std::thread::sleep(std::time::Duration::from_secs(2));

    // List to get the codespace name
    let codespace_name = find_existing_codespace(repo)?
        .ok_or_else(|| anyhow!("Codespace created but not found in list"))?;

    // Wait for it to be in Available state
    eprintln!("Waiting for codespace to be ready...");
    for _ in 0..30 {
        let output = Command::new("gh")
            .args([
                "codespace",
                "view",
                "--codespace",
                &codespace_name,
                "--json",
                "state",
            ])
            .output()
            .context("Failed to check codespace state")?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(state) = serde_json::from_str::<serde_json::Value>(&stdout) {
                if state.get("state").and_then(|s| s.as_str()) == Some("Available") {
                    return Ok(codespace_name);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    Ok(codespace_name)
}

/// Ensure a codespace is running (start it if stopped).
fn ensure_codespace_running(codespace_name: &str) -> Result<()> {
    // Check current state
    let output = Command::new("gh")
        .args([
            "codespace",
            "view",
            "--codespace",
            codespace_name,
            "--json",
            "state",
        ])
        .output()
        .context("Failed to check codespace state")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to check codespace state: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let state: serde_json::Value = serde_json::from_str(&stdout)
        .context("Failed to parse codespace state")?;

    let current_state = state
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("Unknown");

    if current_state == "Available" {
        debug!(codespace = %codespace_name, "codespace already running");
        return Ok(());
    }

    // Start the codespace
    info!(codespace = %codespace_name, state = %current_state, "starting codespace");
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

/// Set up SSH config for a codespace.
///
/// Uses `gh codespace ssh --config` to get SSH config and writes to ~/.ssh/config.
/// Returns the SSH host alias.
fn setup_ssh(codespace_name: &str) -> Result<String> {
    // Get SSH config from gh
    let output = Command::new("gh")
        .args(["codespace", "ssh", "--config", "--codespace", codespace_name])
        .output()
        .context("Failed to get SSH config")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to get SSH config: {}", stderr.trim()));
    }

    let config_entry = String::from_utf8_lossy(&output.stdout);

    // Extract the Host line to get the actual hostname
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

    // Read existing config
    let existing_config = std::fs::read_to_string(&ssh_config_path).unwrap_or_default();

    // Check if this host is already configured
    if !existing_config.contains(&format!("Host {}", ssh_host)) {
        // Append the new entry
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ssh_config_path)
            .context("Failed to open SSH config")?;

        use std::io::Write;
        writeln!(file, "\n{}", config_entry.trim())?;
        debug!(ssh_host = %ssh_host, "added SSH config entry");
    }

    Ok(ssh_host)
}

/// Run a command on a codespace via SSH.
pub fn run_remote(
    ssh_host: &str,
    remote_workdir: &Path,
    command: &str,
) -> Result<std::process::Output> {
    let full_command = format!(
        "cd {} && {}",
        remote_workdir.display(),
        command
    );

    debug!(
        ssh_host = %ssh_host,
        workdir = %remote_workdir.display(),
        command = %command,
        "running remote command"
    );

    Command::new("ssh")
        .args([ssh_host, &full_command])
        .output()
        .context("Failed to execute remote command")
}

/// Check if SSH connection to a codespace works.
pub fn check_ssh(ssh_host: &str) -> Result<bool> {
    let output = Command::new("ssh")
        .args(["-o", "ConnectTimeout=5", ssh_host, "true"])
        .output()
        .context("Failed to check SSH connection")?;

    Ok(output.status.success())
}
