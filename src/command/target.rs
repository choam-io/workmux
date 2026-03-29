//! CLI command for managing execution targets.
//!
//! ```
//! # Attach a codespace to current worktree
//! workmux target attach --codespace acme-corp/webapp
//!
//! # Show current target
//! workmux target status
//!
//! # Detach (stop codespace)
//! workmux target detach
//! ```

use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::target::{TargetStore, TargetState, TargetType};
use crate::target::codespace;
use crate::workflow;

#[derive(Args)]
pub struct TargetArgs {
    #[command(subcommand)]
    pub command: TargetCommand,
}

#[derive(Subcommand)]
pub enum TargetCommand {
    /// Attach an execution target to the current worktree
    Attach {
        /// Attach a GitHub Codespace for the given repo (e.g., acme-corp/webapp)
        #[arg(long)]
        codespace: Option<String>,
    },

    /// Detach the execution target from the current worktree
    Detach,

    /// Show the current execution target
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: TargetArgs) -> Result<()> {
    match args.command {
        TargetCommand::Attach { codespace } => run_attach(codespace),
        TargetCommand::Detach => run_detach(),
        TargetCommand::Status { json } => run_status(json),
    }
}

fn run_attach(codespace_repo: Option<String>) -> Result<()> {
    let repo = codespace_repo.ok_or_else(|| {
        anyhow!("Must specify a target type. Use --codespace <repo>")
    })?;

    // Resolve current worktree
    let cwd = std::env::current_dir()?;
    let worktree_path = workflow::find_worktree_root(&cwd)
        .ok_or_else(|| anyhow!("Not in a workmux worktree"))?;

    let handle = worktree_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("Could not determine worktree handle"))?
        .to_string();

    let store = TargetStore::new()?;

    // Check if already attached
    if let Some(existing) = store.load(&handle)? {
        if !existing.is_local() {
            return Err(anyhow!(
                "Worktree already has a target attached. Run 'workmux target detach' first."
            ));
        }
    }

    println!("Attaching codespace for {} to worktree {}...", repo, handle);

    // Create/reuse codespace and set up SSH
    let (codespace_name, ssh_host) = codespace::attach_codespace(&repo, &worktree_path)?;

    // Determine remote working directory
    let repo_name = repo.split('/').last().unwrap_or(&repo);
    let remote_workdir = PathBuf::from(format!("/workspaces/{}", repo_name));

    // Save target state
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let state = TargetState {
        handle: handle.clone(),
        worktree_path: worktree_path.clone(),
        target: TargetType::Codespace {
            repo: repo.clone(),
            codespace_name: codespace_name.clone(),
            ssh_host: ssh_host.clone(),
        },
        attached_ts: now,
        remote_workdir: Some(remote_workdir.clone()),
    };

    store.save(&state)?;

    println!();
    println!("✓ Target attached:");
    println!("  Codespace: {}", codespace_name);
    println!("  SSH host:  {}", ssh_host);
    println!("  Remote:    {}", remote_workdir.display());
    println!();
    println!("Run commands with: workmux run -- <command>");

    Ok(())
}

fn run_detach() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let worktree_path = workflow::find_worktree_root(&cwd)
        .ok_or_else(|| anyhow!("Not in a workmux worktree"))?;

    let handle = worktree_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("Could not determine worktree handle"))?
        .to_string();

    let store = TargetStore::new()?;

    let state = store.load(&handle)?
        .ok_or_else(|| anyhow!("No target attached to this worktree"))?;

    if state.is_local() {
        return Err(anyhow!("No remote target attached to this worktree"));
    }

    println!("Detaching target from worktree {}...", handle);

    // Stop the codespace
    if let Some(codespace_name) = state.codespace_name() {
        codespace::detach_codespace(codespace_name)?;
    }

    // Remove state
    store.delete(&handle)?;

    println!("✓ Target detached");

    Ok(())
}

fn run_status(json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let worktree_path = workflow::find_worktree_root(&cwd)
        .ok_or_else(|| anyhow!("Not in a workmux worktree"))?;

    let handle = worktree_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("Could not determine worktree handle"))?
        .to_string();

    let store = TargetStore::new()?;

    match store.load(&handle)? {
        Some(state) if !state.is_local() => {
            if json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                match &state.target {
                    TargetType::Codespace { repo, codespace_name, ssh_host } => {
                        println!("Target: codespace");
                        println!("  Repository: {}", repo);
                        println!("  Codespace:  {}", codespace_name);
                        println!("  SSH host:   {}", ssh_host);
                        if let Some(ref workdir) = state.remote_workdir {
                            println!("  Remote dir: {}", workdir.display());
                        }
                    }
                    TargetType::Local => unreachable!(),
                }
            }
        }
        _ => {
            if json {
                println!(r#"{{"type": "local"}}"#);
            } else {
                println!("Target: local (no remote target attached)");
            }
        }
    }

    Ok(())
}
