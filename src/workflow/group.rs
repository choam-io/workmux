//! Group workflow: cross-repo worktrees with a single agent.
//!
//! Groups allow creating worktrees across multiple repositories and giving
//! one agent a unified view of all of them via symlinks in a workspace directory.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use crate::config::{Config, MuxMode};
use crate::git;
use crate::multiplexer::{MuxHandle, Multiplexer, create_backend, detect_backend};
use crate::prompt::Prompt;


/// Default directory for group workspaces
const GROUPS_DIR: &str = ".local/share/workmux/groups";

/// State file name within each group workspace
pub const STATE_FILE: &str = ".workmux-group.yaml";

/// State for a single repository in a group
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRepoState {
    /// Original repository path (expanded)
    pub repo_path: PathBuf,
    /// Path to the created worktree
    pub worktree_path: PathBuf,
    /// Branch name used
    pub branch: String,
    /// Symlink name in workspace (repo directory name)
    pub symlink_name: String,
}

/// Persisted state for a group workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupState {
    /// Group name from config
    pub group_name: String,
    /// Branch used across all repos
    pub branch: String,
    /// State for each repository
    pub repos: Vec<GroupRepoState>,
    /// Merge order (repo directory names)
    #[serde(default)]
    pub merge_order: Vec<String>,
    /// Unix timestamp when created
    pub created_at: u64,
    /// Whether this was created in headless mode
    #[serde(default)]
    pub headless: bool,
    /// Dev environment state (codespace + tunnels + port mappings)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_env: Option<crate::dev_env::DevEnvState>,
}

impl GroupState {
    /// Load state from a workspace directory
    pub fn load(workspace_dir: &Path) -> Result<Self> {
        let state_path = workspace_dir.join(STATE_FILE);
        let content = fs::read_to_string(&state_path)
            .with_context(|| format!("Failed to read group state from {}", state_path.display()))?;
        serde_yaml::from_str(&content).context("Failed to parse group state YAML")
    }

    /// Save state to a workspace directory
    pub fn save(&self, workspace_dir: &Path) -> Result<()> {
        let state_path = workspace_dir.join(STATE_FILE);
        let content = serde_yaml::to_string(self).context("Failed to serialize group state")?;
        fs::write(&state_path, content)
            .with_context(|| format!("Failed to write group state to {}", state_path.display()))?;
        Ok(())
    }
}

/// Get the groups directory path
pub fn groups_dir() -> Result<PathBuf> {
    let home = home::home_dir().ok_or_else(|| anyhow!("Could not determine home directory"))?;
    Ok(home.join(GROUPS_DIR))
}

/// Get workspace directory for a group/branch combination
pub fn workspace_dir(group_name: &str, branch: &str) -> Result<PathBuf> {
    let slug_branch = slug::slugify(branch);
    let dir_name = format!("{}--{}", group_name, slug_branch);
    Ok(groups_dir()?.join(dir_name))
}

/// Derive the mux window handle for a group workspace.
/// Uses the same naming logic as regular worktrees (derive_handle from branch).
fn derive_group_handle(branch: &str, config: &Config) -> Result<String> {
    crate::naming::derive_handle(branch, None, config)
}

/// Get the mux prefix for window names. Uses the config method which
/// respects nerdfont detection (glyph prefix when available).
fn mux_prefix(config: &Config) -> &str {
    config.window_prefix()
}

/// Expand tilde in path
fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home::home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = home::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// Arguments for group add
pub struct GroupAddArgs<'a> {
    pub group_name: &'a str,
    pub branch: &'a str,
    pub prompt: Option<&'a Prompt>,
    pub background: bool,
    pub headless: bool,
}

/// Result from group add
pub struct GroupAddResult {
    pub workspace_dir: PathBuf,
    pub repos_created: usize,
    pub state: GroupState,
}

/// Create worktrees across all repos in a group
pub fn add(config: &Config, args: GroupAddArgs) -> Result<GroupAddResult> {
    let GroupAddArgs {
        group_name,
        branch,
        prompt,
        background,
        headless,
    } = args;

    info!(group = group_name, branch = branch, "group:add:start");

    // Look up group config
    let group_config = config
        .groups
        .as_ref()
        .and_then(|g| g.get(group_name))
        .ok_or_else(|| {
            anyhow!(
                "Group '{}' not found in config. Define it in ~/.config/workmux/config.yaml",
                group_name
            )
        })?;

    if group_config.repos.is_empty() {
        bail!("Group '{}' has no repositories defined", group_name);
    }

    // Create workspace directory
    let ws_dir = workspace_dir(group_name, branch)?;
    if ws_dir.exists() {
        bail!(
            "Group workspace already exists: {}\nUse 'workmux group remove {} {}' to clean up first.",
            ws_dir.display(),
            group_name,
            branch
        );
    }
    fs::create_dir_all(&ws_dir)
        .with_context(|| format!("Failed to create workspace directory: {}", ws_dir.display()))?;

    let mut repo_states = Vec::new();
    let mut errors = Vec::new();

    // Create worktree in each repo
    for repo_config in &group_config.repos {
        let repo_path = expand_path(&repo_config.path);
        let repo_name = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        debug!(repo = %repo_path.display(), "group:add:creating worktree");

        match create_worktree_in_repo(&repo_path, branch) {
            Ok(worktree_path) => {
                // Create symlink in workspace
                let symlink_path = ws_dir.join(&repo_name);
                if let Err(e) = std::os::unix::fs::symlink(&worktree_path, &symlink_path) {
                    warn!(
                        repo = repo_name,
                        error = %e,
                        "group:add:failed to create symlink"
                    );
                    errors.push(format!("{}: symlink failed: {}", repo_name, e));
                    continue;
                }

                repo_states.push(GroupRepoState {
                    repo_path: repo_path.clone(),
                    worktree_path,
                    branch: branch.to_string(),
                    symlink_name: repo_name,
                });
            }
            Err(e) => {
                warn!(repo = repo_name, error = %e, "group:add:failed to create worktree");
                errors.push(format!("{}: {}", repo_name, e));
            }
        }
    }

    if repo_states.is_empty() {
        // Clean up empty workspace
        let _ = fs::remove_dir_all(&ws_dir);
        bail!(
            "Failed to create any worktrees:\n{}",
            errors.join("\n")
        );
    }

    // Determine merge order
    let merge_order = group_config
        .merge_order
        .clone()
        .unwrap_or_else(|| repo_states.iter().map(|r| r.symlink_name.clone()).collect());

    // Create state
    let state = GroupState {
        group_name: group_name.to_string(),
        branch: branch.to_string(),
        repos: repo_states.clone(),
        merge_order,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        headless,
        dev_env: None,
    };
    state.save(&ws_dir)?;

    // Write prompt file if provided
    if let Some(p) = prompt {
        let prompt_path = ws_dir.join(".workmux").join("PROMPT.md");
        if let Some(parent) = prompt_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = p.read_content()?;
        fs::write(&prompt_path, &content)?;
    }

    // Launch agent unless headless
    if !headless {
        let mux = create_backend(detect_backend());
        if mux.is_running()? {
            let repo_names: Vec<String> = repo_states.iter().map(|r| r.symlink_name.clone()).collect();
            launch_group_agent(&ws_dir, group_name, branch, &repo_names, prompt, background, mux.as_ref())?;
        } else if !background {
            eprintln!(
                "workmux: no multiplexer running, created workspace at {}",
                ws_dir.display()
            );
        }
    }

    // Report partial failures
    if !errors.is_empty() {
        eprintln!(
            "Warning: some repositories failed:\n{}",
            errors.join("\n")
        );
    }

    info!(
        group = group_name,
        branch = branch,
        repos = repo_states.len(),
        "group:add:complete"
    );

    Ok(GroupAddResult {
        workspace_dir: ws_dir,
        repos_created: repo_states.len(),
        state,
    })
}

/// Create a worktree in a specific repository
fn create_worktree_in_repo(repo_path: &Path, branch: &str) -> Result<PathBuf> {
    if !repo_path.exists() {
        bail!("Repository path does not exist: {}", repo_path.display());
    }

    // Get the worktrees directory for this repo
    let repo_name = repo_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let worktrees_dir = repo_path
        .parent()
        .ok_or_else(|| anyhow!("Could not determine parent directory"))?
        .join(format!("{}__worktrees", repo_name));

    let worktree_path = worktrees_dir.join(slug::slugify(branch));

    // Check if worktree already exists
    let worktrees = git::list_worktrees_in(Some(repo_path))?;
    for (path, wt_branch) in &worktrees {
        if wt_branch == branch {
            // Worktree exists for this branch, return its path
            return Ok(path.clone());
        }
    }

    // Check if branch exists
    let branch_exists = git::branch_exists_in(branch, Some(repo_path))?;

    // Get current branch to use as base
    let base_branch = if !branch_exists {
        Some(git::get_current_branch_in(repo_path)?)
    } else {
        None
    };

    // Create worktree
    git::create_worktree_in(
        repo_path,
        &worktree_path,
        branch,
        !branch_exists,
        base_branch.as_deref(),
    )?;

    Ok(worktree_path)
}

/// Launch an agent in the group workspace
fn launch_group_agent(
    workspace_dir: &Path,
    group_name: &str,
    branch: &str,
    repo_names: &[String],
    prompt: Option<&Prompt>,
    background: bool,
    mux: &dyn Multiplexer,
) -> Result<()> {
    // Load config for agent setup
    let config = Config::load(None)?;

    // Derive handle from branch name using the same naming as regular worktrees
    let handle = derive_group_handle(branch, &config)?;
    let prefix = mux_prefix(&config);

    // Get agent command and profile
    let agent_cmd = config.agent.as_deref().unwrap_or("claude");
    let agent_profile = crate::multiplexer::agent::resolve_profile(Some(agent_cmd));

    let mux_handle = MuxHandle::new(mux, MuxMode::Window, prefix, &handle);

    if mux_handle.exists()? {
        if !background {
            mux_handle.select()?;
        }
        return Ok(());
    }

    use crate::multiplexer::CreateWindowParams;
    let surface_ref = mux.create_window(CreateWindowParams {
        prefix,
        name: &handle,
        cwd: workspace_dir,
        after_window: None,
    })?;

    // Set group info status pill on the NEW workspace (resolve via surface ref)
    set_group_info_status(&surface_ref, group_name, repo_names);

    // Build agent command with prompt injection if provided
    let agent_command = if prompt.is_some() {
        // The prompt file was already written to .workmux/PROMPT.md in add()
        let prompt_path = workspace_dir.join(".workmux").join("PROMPT.md");
        let prompt_arg = agent_profile.prompt_argument(&prompt_path.to_string_lossy());
        format!("{} {}", agent_cmd, prompt_arg)
    } else {
        agent_cmd.to_string()
    };

    // Wait for shell to be ready, then send the agent command
    for _ in 0..30 {
        if let Some(screen) = mux.capture_pane(&surface_ref, 5) {
            if !screen.trim().is_empty() {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    mux.send_keys(&surface_ref, &agent_command)?;

    // Set working status for agents that need it when launched with a prompt
    if prompt.is_some() && agent_profile.needs_auto_status() {
        let icon = config.status_icons.working();
        if config.status_format.unwrap_or(true) {
            let _ = mux.ensure_status_format(&surface_ref);
        }
        let _ = mux.set_status(&surface_ref, icon, false);
    }

    if !background {
        mux_handle.select()?;
    }

    Ok(())
}

/// Set a status pill showing group name and repos on the new group workspace.
/// Resolves the workspace ref from the surface ref via `cmux tree`.
fn set_group_info_status(surface_ref: &str, group_name: &str, repo_names: &[String]) {
    let ws_ref = resolve_workspace_for_surface(surface_ref);
    let label = format!("{}: {}", group_name, repo_names.join(", "));
    let mut args = vec![
        "set-status", "workmux_group", &label,
        "--icon", "folder.fill",
        "--color", "#8B5CF6",
    ];
    let ws_val;
    if let Some(ref ws) = ws_ref {
        ws_val = ws.clone();
        args.push("--workspace");
        args.push(&ws_val);
    }
    let _ = crate::cmd::Cmd::new("cmux").args(&args).run();
}

/// Parse `cmux tree --all` to find which workspace contains a given surface ref.
fn resolve_workspace_for_surface(surface_ref: &str) -> Option<String> {
    let output = crate::cmd::Cmd::new("cmux")
        .args(&["tree", "--all"])
        .run_and_capture_stdout()
        .ok()?;
    let mut current_ws: Option<String> = None;
    for line in output.lines() {
        let trimmed = line.trim().trim_start_matches(|c: char| !c.is_alphanumeric());
        if let Some(rest) = trimmed.strip_prefix("workspace ") {
            if let Some(ws) = rest.split_whitespace().next() {
                if ws.starts_with("workspace:") {
                    current_ws = Some(ws.to_string());
                }
            }
        }
        if trimmed.contains(surface_ref) {
            return current_ws;
        }
    }
    None
}

/// List all active group workspaces
pub fn list() -> Result<Vec<GroupState>> {
    let dir = groups_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut groups = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if let Ok(state) = GroupState::load(&path) {
                groups.push(state);
            }
        }
    }

    // Sort by creation time (newest first)
    groups.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    Ok(groups)
}

/// Status information for a group
pub struct GroupStatus {
    pub state: GroupState,
    pub workspace_dir: PathBuf,
    pub repo_statuses: Vec<RepoStatus>,
    pub agent_running: bool,
}

/// Status for a single repository in a group
pub struct RepoStatus {
    pub name: String,
    pub worktree_exists: bool,
    pub has_uncommitted: bool,
    pub unmerged_commits: usize,
    pub branch: String,
}

/// Get status for a group
pub fn status(group_name: &str, branch: &str) -> Result<GroupStatus> {
    let ws_dir = workspace_dir(group_name, branch)?;
    if !ws_dir.exists() {
        bail!(
            "Group workspace not found: {}--{}",
            group_name,
            slug::slugify(branch)
        );
    }

    let state = GroupState::load(&ws_dir)?;
    let mut repo_statuses = Vec::new();

    for repo_state in &state.repos {
        let worktree_exists = repo_state.worktree_path.exists();
        let has_uncommitted = if worktree_exists {
            git::has_uncommitted_changes(&repo_state.worktree_path).unwrap_or(false)
        } else {
            false
        };

        // Count unmerged commits (simplified - just check if ahead of main)
        let unmerged_commits = if worktree_exists {
            count_unmerged_commits(&repo_state.worktree_path).unwrap_or(0)
        } else {
            0
        };

        repo_statuses.push(RepoStatus {
            name: repo_state.symlink_name.clone(),
            worktree_exists,
            has_uncommitted,
            unmerged_commits,
            branch: repo_state.branch.clone(),
        });
    }

    // Check if agent window exists
    let mux = create_backend(detect_backend());
    let config = Config::load(None)?;
    let handle = derive_group_handle(branch, &config)?;
    let prefix = mux_prefix(&config);
    let mux_handle = MuxHandle::new(mux.as_ref(), MuxMode::Window, prefix, &handle);
    let agent_running = mux_handle.exists().unwrap_or(false);

    Ok(GroupStatus {
        state,
        workspace_dir: ws_dir,
        repo_statuses,
        agent_running,
    })
}

fn count_unmerged_commits(worktree_path: &Path) -> Result<usize> {
    use crate::cmd::Cmd;

    let output = Cmd::new("git")
        .workdir(worktree_path)
        .args(&["rev-list", "--count", "HEAD", "^origin/HEAD"])
        .run_and_capture_stdout()
        .unwrap_or_else(|_| "0".to_string());

    Ok(output.trim().parse().unwrap_or(0))
}

/// Merge arguments
pub struct GroupMergeArgs<'a> {
    pub group_name: &'a str,
    pub branch: &'a str,
    pub into: Option<&'a str>,
    pub keep: bool,
}

/// Merge all branches in a group (in order) and clean up
pub fn merge(args: GroupMergeArgs) -> Result<()> {
    let GroupMergeArgs {
        group_name,
        branch,
        into,
        keep,
    } = args;

    let ws_dir = workspace_dir(group_name, branch)?;
    if !ws_dir.exists() {
        bail!(
            "Group workspace not found: {}--{}",
            group_name,
            slug::slugify(branch)
        );
    }

    let state = GroupState::load(&ws_dir)?;
    info!(
        group = group_name,
        branch = branch,
        repos = state.repos.len(),
        "group:merge:start"
    );

    // Build lookup map for repos by symlink name
    let repo_map: HashMap<_, _> = state
        .repos
        .iter()
        .map(|r| (r.symlink_name.clone(), r))
        .collect();

    // Merge in order
    let mut merged = Vec::new();
    let mut errors = Vec::new();

    for repo_name in &state.merge_order {
        if let Some(repo_state) = repo_map.get(repo_name) {
            debug!(repo = repo_name, "group:merge:merging");

            match merge_repo_worktree(repo_state, into) {
                Ok(()) => {
                    merged.push(repo_name.clone());
                    println!("✓ Merged: {}", repo_name);
                }
                Err(e) => {
                    errors.push(format!("{}: {}", repo_name, e));
                    eprintln!("✗ Failed to merge {}: {}", repo_name, e);
                }
            }
        }
    }

    // Handle repos not in merge_order
    for repo_state in &state.repos {
        if !state.merge_order.contains(&repo_state.symlink_name) {
            debug!(repo = repo_state.symlink_name, "group:merge:merging (unordered)");

            match merge_repo_worktree(repo_state, into) {
                Ok(()) => {
                    merged.push(repo_state.symlink_name.clone());
                    println!("✓ Merged: {}", repo_state.symlink_name);
                }
                Err(e) => {
                    errors.push(format!("{}: {}", repo_state.symlink_name, e));
                    eprintln!("✗ Failed to merge {}: {}", repo_state.symlink_name, e);
                }
            }
        }
    }

    // Clean up unless --keep
    if !keep {
        remove_internal(group_name, branch, true)?;
    }

    if !errors.is_empty() {
        bail!(
            "Some merges failed:\n{}",
            errors.join("\n")
        );
    }

    info!(
        group = group_name,
        branch = branch,
        merged = merged.len(),
        "group:merge:complete"
    );

    Ok(())
}

fn merge_repo_worktree(repo_state: &GroupRepoState, into: Option<&str>) -> Result<()> {
    use crate::cmd::Cmd;

    let worktree_path = &repo_state.worktree_path;
    if !worktree_path.exists() {
        bail!("Worktree does not exist");
    }

    // Get the target branch
    let target = if let Some(t) = into {
        t.to_string()
    } else {
        // Get default branch from repo
        git::get_default_branch_in(Some(&repo_state.repo_path))?
    };

    // Check for uncommitted changes
    if git::has_uncommitted_changes(worktree_path)? {
        bail!("Has uncommitted changes");
    }

    // Switch to main worktree to merge
    let main_worktree = git::get_main_worktree_root_in(&repo_state.repo_path)?;

    // Checkout target branch
    Cmd::new("git")
        .workdir(&main_worktree)
        .args(&["checkout", &target])
        .run()
        .context("Failed to checkout target branch")?;

    // Merge
    Cmd::new("git")
        .workdir(&main_worktree)
        .args(&["merge", &repo_state.branch, "--no-edit"])
        .run()
        .context("Failed to merge branch")?;

    // Remove worktree
    Cmd::new("git")
        .workdir(&repo_state.repo_path)
        .args(&["worktree", "remove", worktree_path.to_str().unwrap()])
        .run()
        .context("Failed to remove worktree")?;

    // Delete branch
    Cmd::new("git")
        .workdir(&repo_state.repo_path)
        .args(&["branch", "-d", &repo_state.branch])
        .run()
        .context("Failed to delete branch")?;

    Ok(())
}

/// Remove a group workspace
pub fn remove(group_name: &str, branch: &str, force: bool) -> Result<()> {
    remove_internal(group_name, branch, force)
}

fn remove_internal(group_name: &str, branch: &str, force: bool) -> Result<()> {
    let ws_dir = workspace_dir(group_name, branch)?;
    if !ws_dir.exists() {
        bail!(
            "Group workspace not found: {}--{}",
            group_name,
            slug::slugify(branch)
        );
    }

    let state = GroupState::load(&ws_dir)?;
    info!(
        group = group_name,
        branch = branch,
        repos = state.repos.len(),
        "group:remove:start"
    );

    // Check for uncommitted changes unless force
    if !force {
        for repo_state in &state.repos {
            if repo_state.worktree_path.exists()
                && git::has_uncommitted_changes(&repo_state.worktree_path)?
            {
                bail!(
                    "Repository '{}' has uncommitted changes. Use -f to force removal.",
                    repo_state.symlink_name
                );
            }
        }
    }

    // Close mux window if exists
    let mux = create_backend(detect_backend());
    let config = Config::load(None)?;
    let handle = derive_group_handle(branch, &config)?;
    let prefix = mux_prefix(&config);
    let mux_handle = MuxHandle::new(mux.as_ref(), MuxMode::Window, prefix, &handle);
    if mux_handle.exists()? {
        MuxHandle::kill_full(mux.as_ref(), MuxMode::Window, &mux_handle.full_name())?;
    }

    // Remove worktrees in each repo
    let mut errors = Vec::new();
    for repo_state in &state.repos {
        if repo_state.worktree_path.exists() {
            debug!(
                repo = repo_state.symlink_name,
                path = %repo_state.worktree_path.display(),
                "group:remove:removing worktree"
            );

            if let Err(e) = remove_worktree_and_branch(&repo_state) {
                errors.push(format!("{}: {}", repo_state.symlink_name, e));
            }
        }
    }

    // Remove workspace directory
    fs::remove_dir_all(&ws_dir)
        .with_context(|| format!("Failed to remove workspace directory: {}", ws_dir.display()))?;

    if !errors.is_empty() {
        eprintln!(
            "Warning: some cleanup failed:\n{}",
            errors.join("\n")
        );
    }

    info!(
        group = group_name,
        branch = branch,
        "group:remove:complete"
    );

    Ok(())
}

fn remove_worktree_and_branch(repo_state: &GroupRepoState) -> Result<()> {
    use crate::cmd::Cmd;

    // Remove worktree
    let worktree_str = repo_state
        .worktree_path
        .to_str()
        .ok_or_else(|| anyhow!("Invalid worktree path"))?;

    Cmd::new("git")
        .workdir(&repo_state.repo_path)
        .args(&["worktree", "remove", "--force", worktree_str])
        .run()
        .with_context(|| format!("Failed to remove worktree: {}", worktree_str))?;

    // Delete branch
    Cmd::new("git")
        .workdir(&repo_state.repo_path)
        .args(&["branch", "-D", &repo_state.branch])
        .run()
        .with_context(|| format!("Failed to delete branch: {}", repo_state.branch))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_workspace_dir_generation() {
        let dir = workspace_dir("choam", "feat/tabbed-popup").unwrap();
        assert!(dir.to_string_lossy().contains("choam--feat-tabbed-popup"));
    }

    #[test]
    fn test_workspace_dir_slugifies_branch() {
        let dir1 = workspace_dir("test", "feature/foo").unwrap();
        let dir2 = workspace_dir("test", "feature_bar").unwrap();

        // Branch names are slugified
        assert!(dir1.to_string_lossy().ends_with("test--feature-foo"));
        assert!(dir2.to_string_lossy().ends_with("test--feature-bar"));
    }

    #[test]
    fn test_expand_path_tilde() {
        let home = home::home_dir().unwrap();

        let path = expand_path("~/test");
        assert_eq!(path, home.join("test"));

        let path = expand_path("~");
        assert_eq!(path, home);

        // Absolute paths unchanged
        let path = expand_path("/tmp/test");
        assert_eq!(path, PathBuf::from("/tmp/test"));

        // Relative paths unchanged
        let path = expand_path("relative/path");
        assert_eq!(path, PathBuf::from("relative/path"));
    }

    #[test]
    fn test_group_state_roundtrip() {
        let tmp = TempDir::new().unwrap();

        let state = GroupState {
            group_name: "test-group".to_string(),
            branch: "feat/test".to_string(),
            repos: vec![GroupRepoState {
                repo_path: PathBuf::from("/home/user/repo1"),
                worktree_path: PathBuf::from("/home/user/repo1__worktrees/feat-test"),
                branch: "feat/test".to_string(),
                symlink_name: "repo1".to_string(),
            }],
            merge_order: vec!["repo1".to_string()],
            created_at: 1234567890,
            headless: false,
            dev_env: None,
        };

        state.save(tmp.path()).unwrap();
        let loaded = GroupState::load(tmp.path()).unwrap();

        assert_eq!(loaded.group_name, state.group_name);
        assert_eq!(loaded.branch, state.branch);
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos[0].symlink_name, "repo1");
        assert_eq!(loaded.merge_order, vec!["repo1"]);
        assert_eq!(loaded.created_at, 1234567890);
        assert!(!loaded.headless);
    }

    #[test]
    fn test_group_state_headless_flag() {
        let tmp = TempDir::new().unwrap();

        let state = GroupState {
            group_name: "headless-test".to_string(),
            branch: "main".to_string(),
            repos: vec![],
            merge_order: vec![],
            created_at: 0,
            headless: true,
            dev_env: None,
        };

        state.save(tmp.path()).unwrap();
        let loaded = GroupState::load(tmp.path()).unwrap();

        assert!(loaded.headless);
    }

    #[test]
    fn test_groups_dir() {
        let dir = groups_dir().unwrap();
        let home = home::home_dir().unwrap();
        assert_eq!(dir, home.join(".local/share/workmux/groups"));
    }

    #[test]
    fn test_list_empty_groups_dir() {
        // If groups dir doesn't exist, list returns empty vec
        // This is tested implicitly - groups_dir() returns a path that may not exist
        // and list() should handle that gracefully
        let groups = list().unwrap();
        // We can't assert it's empty because there might be real groups
        // Just verify it doesn't error
        let _ = groups;
    }
}
