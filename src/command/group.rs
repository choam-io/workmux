//! Command handlers for `workmux group` subcommands.

use anyhow::{Result, bail};
use std::io::IsTerminal;
use tabled::{Table, Tabled, settings::{Padding, Style}};

use crate::config::Config;
use crate::util::format_compact_age;
use crate::workflow::group::{self, GroupAddArgs, GroupMergeArgs};
use crate::workflow::prompt_loader::{PromptLoadArgs, load_prompt};

/// Run `workmux group add`
pub fn run_add(
    group_name: &str,
    branch: &str,
    prompt_inline: Option<&str>,
    prompt_file: Option<&std::path::Path>,
    prompt_editor: bool,
    background: bool,
    headless: bool,
) -> Result<()> {
    let config = Config::load(None)?;

    // Load prompt if provided
    let prompt = load_prompt(&PromptLoadArgs {
        prompt_editor,
        prompt_inline,
        prompt_file: prompt_file.map(|p| p.to_path_buf()).as_ref(),
    })?;

    let result = group::add(
        &config,
        GroupAddArgs {
            group_name,
            branch,
            prompt: prompt.as_ref(),
            background,
            headless,
        },
    )?;

    println!(
        "✓ Created group workspace: {}",
        result.workspace_dir.display()
    );
    println!("  Repositories: {}", result.repos_created);
    println!("  Branch: {}", result.state.branch);

    Ok(())
}

/// Table row for group list
#[derive(Tabled)]
struct GroupRow {
    #[tabled(rename = "GROUP")]
    group: String,
    #[tabled(rename = "BRANCH")]
    branch: String,
    #[tabled(rename = "REPOS")]
    repos: String,
    #[tabled(rename = "AGE")]
    age: String,
}

/// Run `workmux group list`
pub fn run_list(json: bool) -> Result<()> {
    let groups = group::list()?;

    if groups.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No active group workspaces");
        }
        return Ok(());
    }

    if json {
        let json_output: Vec<serde_json::Value> = groups
            .iter()
            .map(|g| {
                serde_json::json!({
                    "group_name": g.group_name,
                    "branch": g.branch,
                    "repos": g.repos.iter().map(|r| &r.symlink_name).collect::<Vec<_>>(),
                    "created_at": g.created_at,
                    "headless": g.headless,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_output)?);
        return Ok(());
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let rows: Vec<GroupRow> = groups
        .iter()
        .map(|g| GroupRow {
            group: g.group_name.clone(),
            branch: g.branch.clone(),
            repos: g.repos.len().to_string(),
            age: format_compact_age(now.saturating_sub(g.created_at)),
        })
        .collect();

    let mut table = Table::new(rows);
    table
        .with(Style::blank())
        .modify(tabled::settings::object::Columns::new(0..4), Padding::new(0, 2, 0, 0));

    println!("{table}");
    Ok(())
}

/// Table row for repo status
#[derive(Tabled)]
struct RepoStatusRow {
    #[tabled(rename = "REPO")]
    name: String,
    #[tabled(rename = "STATUS")]
    status: String,
    #[tabled(rename = "CHANGES")]
    changes: String,
    #[tabled(rename = "UNMERGED")]
    unmerged: String,
}

/// Run `workmux group status`
pub fn run_status(group_name: &str, branch: &str, json: bool) -> Result<()> {
    let status = group::status(group_name, branch)?;

    if json {
        let json_output = serde_json::json!({
            "group_name": status.state.group_name,
            "branch": status.state.branch,
            "workspace_dir": status.workspace_dir.to_string_lossy(),
            "agent_running": status.agent_running,
            "repos": status.repo_statuses.iter().map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "worktree_exists": r.worktree_exists,
                    "has_uncommitted": r.has_uncommitted,
                    "unmerged_commits": r.unmerged_commits,
                    "branch": r.branch,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&json_output)?);
        return Ok(());
    }

    println!("Group: {} (branch: {})", status.state.group_name, status.state.branch);
    println!("Workspace: {}", status.workspace_dir.display());
    println!(
        "Agent: {}",
        if status.agent_running { "running" } else { "stopped" }
    );
    println!();

    let rows: Vec<RepoStatusRow> = status
        .repo_statuses
        .iter()
        .map(|r| RepoStatusRow {
            name: r.name.clone(),
            status: if r.worktree_exists { "✓" } else { "✗" }.to_string(),
            changes: if r.has_uncommitted { "●" } else { "-" }.to_string(),
            unmerged: if r.unmerged_commits > 0 {
                r.unmerged_commits.to_string()
            } else {
                "-".to_string()
            },
        })
        .collect();

    let mut table = Table::new(rows);
    table
        .with(Style::blank())
        .modify(tabled::settings::object::Columns::new(0..4), Padding::new(0, 2, 0, 0));

    println!("{table}");
    Ok(())
}

/// Run `workmux group merge`
pub fn run_merge(group_name: &str, branch: &str, into: Option<&str>, keep: bool) -> Result<()> {
    // Confirm unless piped
    if std::io::stdin().is_terminal() {
        eprintln!(
            "This will merge branch '{}' into {} across all repos in group '{}'.",
            branch,
            into.unwrap_or("their default branches"),
            group_name
        );
        eprint!("Continue? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            bail!("Aborted");
        }
    }

    group::merge(GroupMergeArgs {
        group_name,
        branch,
        into,
        keep,
    })?;

    println!("✓ Group merge complete");
    Ok(())
}

/// Run `workmux group remove`
pub fn run_remove(group_name: &str, branch: &str, force: bool) -> Result<()> {
    // Confirm unless force or piped
    if !force && std::io::stdin().is_terminal() {
        eprintln!(
            "This will remove all worktrees for group '{}' branch '{}'.",
            group_name, branch
        );
        eprint!("Continue? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            bail!("Aborted");
        }
    }

    group::remove(group_name, branch, force)?;

    println!("✓ Group workspace removed");
    Ok(())
}
