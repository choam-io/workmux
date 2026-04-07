//! Group workflow: cross-repo worktrees with a single agent.
//!
//! Groups allow creating worktrees across multiple repositories and giving
//! one agent a unified view of all of them via symlinks in a workspace directory.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use crate::config::{Config, MuxMode, ShipStrategy};
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
    /// Ship strategy for this repo (resolved: group default with per-repo override applied)
    #[serde(default)]
    pub ship: ShipStrategy,
}

/// State for a non-git directory symlinked into a group workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDirState {
    /// Absolute path to the directory on the host
    pub path: PathBuf,
    /// Symlink name in workspace (directory basename)
    pub symlink_name: String,
}

/// Persisted state for a group workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupState {
    /// Group name from config
    pub group_name: String,
    /// Branch used across all repos
    pub branch: String,
    /// Default ship strategy for this group
    #[serde(default)]
    pub ship: ShipStrategy,
    /// Freeform context for the agent (injected into system prompt)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// State for each repository
    pub repos: Vec<GroupRepoState>,
    /// Non-git directories symlinked into the workspace.
    /// Each entry is the expanded absolute path to the directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<GroupDirState>,
    /// Unix timestamp when created
    pub created_at: u64,
    /// Dev environment state (codespace + tunnels + port mappings)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_env: Option<crate::dev_env::DevEnvState>,
}

/// Name of the generated VS Code workspace file
const VSCODE_WORKSPACE_SUFFIX: &str = ".code-workspace";

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
    pub no_fetch: bool,
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
        no_fetch,
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

    // Fetch all repos from remote before creating worktrees (unless --no-fetch)
    let repo_paths: Vec<PathBuf> = group_config
        .repos
        .iter()
        .map(|r| expand_path(&r.path))
        .collect();
    if !no_fetch {
        fetch_repos_parallel(&repo_paths);
    }

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

                // Resolve ship strategy: per-repo override > group default
                let ship = repo_config.ship.unwrap_or(group_config.ship);

                repo_states.push(GroupRepoState {
                    repo_path: repo_path.clone(),
                    worktree_path,
                    branch: branch.to_string(),
                    symlink_name: repo_name,
                    ship,
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

    // Symlink non-git directories into the workspace
    let dir_states = link_dirs_into_workspace(group_config.dirs.as_deref(), &ws_dir);

    // Create state
    let mut state = GroupState {
        group_name: group_name.to_string(),
        branch: branch.to_string(),
        ship: group_config.ship,
        context: group_config.context.clone(),
        repos: repo_states.clone(),
        dirs: dir_states,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        dev_env: group_config.dev_env.as_ref().map(|c| {
            crate::dev_env::DevEnvState::from_config(c.clone())
        }),
    };
    state.save(&ws_dir)?;

    // Generate VS Code workspace file
    generate_vscode_workspace(&state, &ws_dir)?;

    // Attach dev environment if configured
    crate::command::dev_env::auto_attach(group_config, &mut state, &ws_dir)?;

    // Write prompt file if provided
    if let Some(p) = prompt {
        let prompt_path = ws_dir.join(".workmux").join("PROMPT.md");
        if let Some(parent) = prompt_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = p.read_content()?;
        fs::write(&prompt_path, &content)?;
    }

    // Launch agent
    {
        let mux = create_backend(detect_backend());
        match mux.ensure_running() {
            Ok(()) => {
                let repo_names: Vec<String> = repo_states.iter().map(|r| r.symlink_name.clone()).collect();
                launch_group_agent(&ws_dir, group_name, branch, &repo_names, prompt, background, false, mux.as_ref())?;
            }
            Err(e) => {
                if !background {
                    eprintln!(
                        "workmux: {}, created workspace at {}",
                        e,
                        ws_dir.display()
                    );
                }
            }
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

/// Generate a VS Code `.code-workspace` file from group state.
///
/// The file lists each repo symlink as a workspace folder so VS Code (and
/// compatible editors) can open the entire group as a multi-root workspace.
fn generate_vscode_workspace(state: &GroupState, workspace_dir: &Path) -> Result<()> {
    let mut folders: Vec<serde_json::Value> = state
        .repos
        .iter()
        .map(|r| {
            serde_json::json!({
                "path": r.symlink_name,
                "name": r.symlink_name,
            })
        })
        .collect();

    // Include linked directories as workspace folders
    for dir in &state.dirs {
        folders.push(serde_json::json!({
            "path": dir.symlink_name,
            "name": dir.symlink_name,
        }));
    }

    let workspace = serde_json::json!({
        "folders": folders,
        "settings": {},
    });

    let filename = format!("{}{}", state.group_name, VSCODE_WORKSPACE_SUFFIX);
    let path = workspace_dir.join(&filename);
    let content = serde_json::to_string_pretty(&workspace)
        .context("Failed to serialize VS Code workspace file")?;
    fs::write(&path, content)
        .with_context(|| format!("Failed to write VS Code workspace file: {}", path.display()))?;

    debug!(path = %path.display(), "group:vscode_workspace:generated");
    Ok(())
}

/// Fetch from origin in multiple repos in parallel.
///
/// Warns on failures but does not bail -- the user might be offline, and local
/// state is still usable. Uses `std::thread::scope` for safe parallel spawning.
fn fetch_repos_parallel(repo_paths: &[PathBuf]) {
    use crate::spinner;

    let repo_count = repo_paths.len();
    let fetch_msg = if repo_count == 1 {
        "Fetching from origin".to_string()
    } else {
        format!("Fetching {} repositories from origin", repo_count)
    };

    let result: Result<Vec<(String, String)>> = spinner::with_spinner(&fetch_msg, || {
        let failures = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for repo_path in repo_paths {
                let failures = &failures;
                s.spawn(move || {
                    let repo_name = repo_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown");
                    if let Err(e) = git::fetch_default_branch_in("origin", repo_path) {
                        failures
                            .lock()
                            .unwrap()
                            .push((repo_name.to_string(), format!("{:#}", e)));
                    }
                });
            }
        });
        Ok(failures.into_inner().unwrap())
    });

    match result {
        Ok(failures) => {
            for (repo, err) in &failures {
                eprintln!("Warning: fetch failed for {}: {}", repo, err);
            }
        }
        Err(e) => {
            eprintln!("Warning: fetch failed ({}), using local state", e);
        }
    }
}

/// Symlink non-git directories into the workspace.
///
/// Each configured dir path is expanded and symlinked by its basename.
/// Missing directories are silently skipped with a warning.
fn link_dirs_into_workspace(dirs: Option<&[String]>, ws_dir: &Path) -> Vec<GroupDirState> {
    let mut dir_states = Vec::new();
    let dirs = match dirs {
        Some(d) => d,
        None => return dir_states,
    };

    for dir_path_str in dirs {
        let dir_path = expand_path(dir_path_str);
        let dir_name = dir_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        if !dir_path.exists() {
            warn!(path = %dir_path.display(), "group:add:dir not found, skipping");
            eprintln!("Warning: directory not found, skipping: {}", dir_path.display());
            continue;
        }

        let symlink_path = ws_dir.join(&dir_name);
        if symlink_path.exists() {
            warn!(name = dir_name, "group:add:symlink name conflict for dir, skipping");
            eprintln!("Warning: symlink '{}' already exists, skipping dir: {}", dir_name, dir_path.display());
            continue;
        }

        if let Err(e) = std::os::unix::fs::symlink(&dir_path, &symlink_path) {
            warn!(dir = dir_name, error = %e, "group:add:failed to symlink dir");
            eprintln!("Warning: failed to symlink dir '{}': {}", dir_name, e);
            continue;
        }

        debug!(dir = %dir_path.display(), name = dir_name, "group:add:linked dir");
        dir_states.push(GroupDirState {
            path: dir_path,
            symlink_name: dir_name,
        });
    }

    dir_states
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

    // Determine base branch: use origin/<default> when available for freshest starting point
    let base_branch = if !branch_exists {
        let default_branch = git::get_default_branch_in(Some(repo_path))?;
        let remote_default = format!("origin/{}", default_branch);
        if git::branch_exists_in(&remote_default, Some(repo_path))? {
            Some(remote_default)
        } else {
            Some(default_branch)
        }
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
/// Arguments for group open
pub struct GroupOpenArgs<'a> {
    pub group_name: &'a str,
    pub branch: &'a str,
    pub prompt: Option<&'a Prompt>,
    pub background: bool,
    pub continue_session: bool,
}

/// Result from group open
pub struct GroupOpenResult {
    pub workspace_dir: PathBuf,
    pub did_switch: bool,
}

/// Open (or switch to) the mux window for an existing group workspace.
///
/// If the window already exists, switches to it. Otherwise creates a new
/// window, launches the agent, and optionally injects a prompt.
pub fn open(args: GroupOpenArgs) -> Result<GroupOpenResult> {
    let GroupOpenArgs {
        group_name,
        branch,
        prompt,
        background,
        continue_session,
    } = args;

    info!(group = group_name, branch = branch, "group:open:start");

    let ws_dir = workspace_dir(group_name, branch)?;
    if !ws_dir.exists() {
        bail!(
            "Group workspace not found: {}--{}\n\
             Use 'workmux group add {} <branch>' to create one.",
            group_name,
            slug::slugify(branch),
            group_name
        );
    }

    let state = GroupState::load(&ws_dir)?;
    let repo_names: Vec<String> = state.repos.iter().map(|r| r.symlink_name.clone()).collect();

    // Write prompt file if provided
    if let Some(p) = prompt {
        let prompt_path = ws_dir.join(".workmux").join("PROMPT.md");
        if let Some(parent) = prompt_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = p.read_content()?;
        fs::write(&prompt_path, &content)?;
    }

    let mux = create_backend(detect_backend());
    if let Err(e) = mux.ensure_running() {
        if !background {
            eprintln!(
                "workmux: {}, workspace at {}",
                e,
                ws_dir.display()
            );
        }
        return Ok(GroupOpenResult {
            workspace_dir: ws_dir,
            did_switch: false,
        });
    }

    let did_switch = launch_group_agent(
        &ws_dir,
        group_name,
        branch,
        &repo_names,
        prompt,
        background,
        continue_session,
        mux.as_ref(),
    )?;

    info!(
        group = group_name,
        branch = branch,
        did_switch = did_switch,
        "group:open:complete"
    );

    Ok(GroupOpenResult {
        workspace_dir: ws_dir,
        did_switch,
    })
}

/// Launch an agent in the group workspace.
///
/// Returns `true` if switched to an existing window, `false` if a new one was created.
fn launch_group_agent(
    workspace_dir: &Path,
    group_name: &str,
    branch: &str,
    repo_names: &[String],
    prompt: Option<&Prompt>,
    background: bool,
    continue_session: bool,
    mux: &dyn Multiplexer,
) -> Result<bool> {
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
        return Ok(true);
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

    // Build agent command with prompt/continue injection
    let agent_command = build_agent_command(
        agent_cmd,
        agent_profile,
        workspace_dir,
        prompt,
        continue_session,
    );

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

    Ok(false)
}

/// Build the agent command string with optional prompt and continue flags.
fn build_agent_command(
    agent_cmd: &str,
    agent_profile: &dyn crate::multiplexer::agent::AgentProfile,
    workspace_dir: &Path,
    prompt: Option<&Prompt>,
    continue_session: bool,
) -> String {
    let mut cmd = agent_cmd.to_string();

    if continue_session {
        if let Some(flag) = agent_profile.continue_flag() {
            cmd = format!("{} {}", cmd, flag);
        }
    }

    if prompt.is_some() {
        let prompt_path = workspace_dir.join(".workmux").join("PROMPT.md");
        let prompt_arg = agent_profile.prompt_argument(&prompt_path.to_string_lossy());
        cmd = format!("{} {}", cmd, prompt_arg);
    }

    cmd
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

/// Arguments for group fork
pub struct GroupForkArgs<'a> {
    pub group_name: &'a str,
    pub source_branch: &'a str,
    pub new_branch: &'a str,
    pub prompt: Option<&'a Prompt>,
    pub background: bool,
}

/// Result from group fork
pub struct GroupForkResult {
    pub workspace_dir: PathBuf,
    pub repos_forked: usize,
    pub state: GroupState,
    /// Per-repo warnings (e.g. dirty state transfer failures)
    pub warnings: Vec<String>,
}

/// Fork an existing group workspace into a new branch.
///
/// Each repo's new worktree branches from the source worktree's HEAD (not the
/// default branch). Uncommitted changes (staged, unstaged, and untracked files)
/// are copied to the new worktree. The source workspace is left untouched.
pub fn fork(config: &Config, args: GroupForkArgs) -> Result<GroupForkResult> {
    let GroupForkArgs {
        group_name,
        source_branch,
        new_branch,
        prompt,
        background,
    } = args;

    info!(
        group = group_name,
        source = source_branch,
        target = new_branch,
        "group:fork:start"
    );

    // Load source group state
    let source_ws_dir = workspace_dir(group_name, source_branch)?;
    if !source_ws_dir.exists() {
        bail!(
            "Source group workspace not found: {}--{}\n\
             Use 'workmux group list' to see active workspaces.",
            group_name,
            slug::slugify(source_branch)
        );
    }
    let source_state = GroupState::load(&source_ws_dir)?;

    // Create new workspace directory
    let new_ws_dir = workspace_dir(group_name, new_branch)?;
    if new_ws_dir.exists() {
        bail!(
            "Target group workspace already exists: {}\n\
             Use 'workmux group remove {} {}' to clean up first.",
            new_ws_dir.display(),
            group_name,
            new_branch
        );
    }
    fs::create_dir_all(&new_ws_dir)
        .with_context(|| format!("Failed to create workspace directory: {}", new_ws_dir.display()))?;

    let mut repo_states = Vec::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for source_repo in &source_state.repos {
        let repo_name = &source_repo.symlink_name;
        debug!(repo = repo_name, "group:fork:forking repo");

        match fork_repo_worktree(source_repo, new_branch) {
            Ok((new_repo_state, repo_warnings)) => {
                // Create symlink in new workspace
                let symlink_path = new_ws_dir.join(repo_name);
                if let Err(e) = std::os::unix::fs::symlink(&new_repo_state.worktree_path, &symlink_path) {
                    warn!(repo = repo_name, error = %e, "group:fork:symlink failed");
                    errors.push(format!("{}: symlink failed: {}", repo_name, e));
                    continue;
                }

                warnings.extend(repo_warnings);
                repo_states.push(new_repo_state);
            }
            Err(e) => {
                warn!(repo = repo_name, error = %e, "group:fork:failed");
                errors.push(format!("{}: {}", repo_name, e));
            }
        }
    }

    if repo_states.is_empty() {
        let _ = fs::remove_dir_all(&new_ws_dir);
        bail!(
            "Failed to fork any repositories:\n{}",
            errors.join("\n")
        );
    }

    // Re-link non-git directories from source state
    let mut dir_states = Vec::new();
    for dir_state in &source_state.dirs {
        let symlink_path = new_ws_dir.join(&dir_state.symlink_name);
        if !dir_state.path.exists() {
            warn!(path = %dir_state.path.display(), "group:fork:dir not found, skipping");
            continue;
        }
        if let Err(e) = std::os::unix::fs::symlink(&dir_state.path, &symlink_path) {
            warn!(dir = dir_state.symlink_name, error = %e, "group:fork:failed to symlink dir");
            continue;
        }
        dir_states.push(dir_state.clone());
    }

    // Create state
    let mut state = GroupState {
        group_name: group_name.to_string(),
        branch: new_branch.to_string(),
        ship: source_state.ship,
        context: source_state.context.clone(),
        repos: repo_states.clone(),
        dirs: dir_states,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        dev_env: None,
    };
    state.save(&new_ws_dir)?;

    // Generate VS Code workspace file
    generate_vscode_workspace(&state, &new_ws_dir)?;

    // Attach dev environment if source had one
    if source_state.dev_env.is_some() {
        if let Some(group_config) = config
            .groups
            .as_ref()
            .and_then(|g| g.get(group_name))
        {
            crate::command::dev_env::auto_attach(group_config, &mut state, &new_ws_dir)?;
        }
    }

    // Write prompt file if provided
    if let Some(p) = prompt {
        let prompt_path = new_ws_dir.join(".workmux").join("PROMPT.md");
        if let Some(parent) = prompt_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = p.read_content()?;
        fs::write(&prompt_path, &content)?;
    }

    // Launch agent
    {
        let mux = create_backend(detect_backend());
        match mux.ensure_running() {
            Ok(()) => {
                let repo_names: Vec<String> = repo_states.iter().map(|r| r.symlink_name.clone()).collect();
                launch_group_agent(
                    &new_ws_dir, group_name, new_branch, &repo_names,
                    prompt, background, false, mux.as_ref(),
                )?;
            }
            Err(e) => {
                if !background {
                    eprintln!(
                        "workmux: {}, created workspace at {}",
                        e,
                        new_ws_dir.display()
                    );
                }
            }
        }
    }

    if !errors.is_empty() {
        eprintln!(
            "Warning: some repositories failed:\n{}",
            errors.join("\n")
        );
    }

    info!(
        group = group_name,
        source = source_branch,
        target = new_branch,
        repos = repo_states.len(),
        "group:fork:complete"
    );

    Ok(GroupForkResult {
        workspace_dir: new_ws_dir,
        repos_forked: repo_states.len(),
        state,
        warnings,
    })
}

/// Fork a single repo's worktree: create new branch from source HEAD,
/// then copy dirty state (staged, unstaged, untracked).
fn fork_repo_worktree(
    source_repo: &GroupRepoState,
    new_branch: &str,
) -> Result<(GroupRepoState, Vec<String>)> {
    use crate::cmd::Cmd;

    let source_wt = &source_repo.worktree_path;
    if !source_wt.exists() {
        bail!("Source worktree does not exist: {}", source_wt.display());
    }

    let repo_path = &source_repo.repo_path;
    let repo_name = &source_repo.symlink_name;

    // Get source HEAD commit
    let source_head = Cmd::new("git")
        .workdir(source_wt)
        .args(&["rev-parse", "HEAD"])
        .run_and_capture_stdout()
        .context("Failed to get source HEAD")?;

    // Compute new worktree path
    let worktrees_dir = repo_path
        .parent()
        .ok_or_else(|| anyhow!("Could not determine parent directory"))?
        .join(format!(
            "{}__worktrees",
            repo_path.file_name().and_then(|n| n.to_str()).unwrap_or("repo")
        ));
    let new_wt_path = worktrees_dir.join(slug::slugify(new_branch));

    // Check if branch already exists
    if git::branch_exists_in(new_branch, Some(repo_path))? {
        bail!("Branch '{}' already exists in {}", new_branch, repo_name);
    }

    // Create new worktree from source HEAD
    // We pass source_head as base_branch so `git worktree add -b <new> <path> <source_head>`
    let source_head_ref = source_head.as_str();
    git::create_worktree_in(
        repo_path,
        &new_wt_path,
        new_branch,
        true, // create_branch
        Some(source_head_ref),
    )?;

    // Propagate workmux-base: new branch should share the source branch's base
    if let Ok(base) = git::get_branch_base_in(&source_repo.branch, Some(source_wt)) {
        let _ = git::set_branch_base_in(new_branch, &base, Some(&new_wt_path));
    }

    // Transfer dirty state
    let mut warnings = Vec::new();

    // 1. Staged changes
    if git::has_staged_changes(source_wt).unwrap_or(false) {
        let result = std::process::Command::new("git")
            .current_dir(source_wt)
            .args(["diff", "--cached", "--binary"])
            .output();

        match result {
            Ok(diff_output) if !diff_output.stdout.is_empty() => {
                let apply = std::process::Command::new("git")
                    .current_dir(&new_wt_path)
                    .args(["apply", "--cached"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn();

                match apply {
                    Ok(mut child) => {
                        use std::io::Write;
                        if let Some(ref mut stdin) = child.stdin {
                            if let Err(e) = stdin.write_all(&diff_output.stdout) {
                                warn!(repo = repo_name, error = %e, "group:fork:staged pipe write failed");
                            }
                        }
                        match child.wait_with_output() {
                            Ok(out) if !out.status.success() => {
                                let msg = format!(
                                    "{}: failed to apply staged changes: {}",
                                    repo_name,
                                    String::from_utf8_lossy(&out.stderr).trim()
                                );
                                warn!("{}", msg);
                                warnings.push(msg);
                            }
                            Err(e) => {
                                warnings.push(format!("{}: staged apply error: {}", repo_name, e));
                            }
                            Ok(_) => {
                                // Materialize staged files in the working tree.
                                // git apply --cached updates the index only; new files
                                // won't exist on disk without this.
                                let _ = std::process::Command::new("git")
                                    .current_dir(&new_wt_path)
                                    .args(["checkout-index", "-a", "-f"])
                                    .output();

                                // Remove working tree files that are staged as deleted.
                                // checkout-index won't touch them (they're gone from the
                                // index) but they still exist on disk from worktree creation.
                                if let Ok(deleted) = std::process::Command::new("git")
                                    .current_dir(&new_wt_path)
                                    .args(["diff", "--cached", "--name-only", "--diff-filter=D"])
                                    .output()
                                {
                                    let paths = String::from_utf8_lossy(&deleted.stdout);
                                    for file_path in paths.lines() {
                                        if !file_path.is_empty() {
                                            let _ = fs::remove_file(new_wt_path.join(file_path));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warnings.push(format!("{}: failed to spawn git apply for staged: {}", repo_name, e));
                    }
                }
            }
            _ => {}
        }
    }

    // 2. Unstaged changes (tracked files only)
    if git::has_unstaged_changes(source_wt).unwrap_or(false) {
        let result = std::process::Command::new("git")
            .current_dir(source_wt)
            .args(["diff", "--binary"])
            .output();

        match result {
            Ok(diff_output) if !diff_output.stdout.is_empty() => {
                let apply = std::process::Command::new("git")
                    .current_dir(&new_wt_path)
                    .args(["apply"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn();

                match apply {
                    Ok(mut child) => {
                        use std::io::Write;
                        if let Some(ref mut stdin) = child.stdin {
                            if let Err(e) = stdin.write_all(&diff_output.stdout) {
                                warn!(repo = repo_name, error = %e, "group:fork:unstaged pipe write failed");
                            }
                        }
                        match child.wait_with_output() {
                            Ok(out) if !out.status.success() => {
                                let msg = format!(
                                    "{}: failed to apply unstaged changes: {}",
                                    repo_name,
                                    String::from_utf8_lossy(&out.stderr).trim()
                                );
                                warn!("{}", msg);
                                warnings.push(msg);
                            }
                            Err(e) => {
                                warnings.push(format!("{}: unstaged apply error: {}", repo_name, e));
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        warnings.push(format!("{}: failed to spawn git apply for unstaged: {}", repo_name, e));
                    }
                }
            }
            _ => {}
        }
    }

    // 3. Untracked files
    if git::has_untracked_files(source_wt).unwrap_or(false) {
        let untracked = std::process::Command::new("git")
            .current_dir(source_wt)
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .output();

        if let Ok(output) = untracked {
            let paths = String::from_utf8_lossy(&output.stdout);
            for file_path in paths.split('\0') {
                if file_path.is_empty() {
                    continue;
                }
                let src = source_wt.join(file_path);
                let dst = new_wt_path.join(file_path);

                // Create parent directory if needed
                if let Some(parent) = dst.parent() {
                    let _ = fs::create_dir_all(parent);
                }

                // Preserve symlinks rather than following them
                match fs::symlink_metadata(&src) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        if let Ok(target) = fs::read_link(&src) {
                            if let Err(e) = std::os::unix::fs::symlink(&target, &dst) {
                                warnings.push(format!(
                                    "{}: failed to copy untracked symlink '{}': {}",
                                    repo_name, file_path, e
                                ));
                            }
                        }
                    }
                    _ => {
                        if let Err(e) = fs::copy(&src, &dst) {
                            warnings.push(format!(
                                "{}: failed to copy untracked file '{}': {}",
                                repo_name, file_path, e
                            ));
                        }
                    }
                }
            }
        }
    }

    let new_repo_state = GroupRepoState {
        repo_path: repo_path.clone(),
        worktree_path: new_wt_path,
        branch: new_branch.to_string(),
        symlink_name: repo_name.clone(),
        ship: source_repo.ship,
    };

    Ok((new_repo_state, warnings))
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

    // Merge each repo
    let mut merged = Vec::new();
    let mut errors = Vec::new();

    for repo_state in &state.repos {
        debug!(repo = repo_state.symlink_name, "group:merge:merging");

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

    // Clean up only if ALL repos succeeded and --keep wasn't passed
    if errors.is_empty() && !keep {
        remove_internal(group_name, branch, true)?;
    } else if !errors.is_empty() && !keep {
        eprintln!(
            "\nWorkspace preserved (some repos failed). Retry after fixing, or use:\n  workmux group remove {} {}",
            group_name, branch
        );
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
    use crate::config::ShipStrategy;

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

    match repo_state.ship {
        ShipStrategy::Pr | ShipStrategy::Mq => {
            // Push the branch to origin
            git::push_branch(worktree_path, "origin", &repo_state.branch, true)?;

            // Create PR via gh cli. Use --json to get structured output
            // so we can reliably extract the PR number.
            let pr_result = Cmd::new("gh")
                .workdir(worktree_path)
                .args(&[
                    "pr", "create",
                    "--base", &target,
                    "--head", &repo_state.branch,
                    "--fill",
                ])
                .run_and_capture_stdout();

            let pr_url = match pr_result {
                Ok(output) => output.trim().to_string(),
                Err(e) => {
                    let err_msg = format!("{}", e);
                    // If a PR already exists for this branch, don't delete
                    // the remote branch -- that would orphan the existing PR.
                    let pr_already_exists = err_msg.contains("already exists")
                        || err_msg.contains("A pull request already exists");

                    if !pr_already_exists {
                        // Push succeeded but PR creation failed for another reason.
                        // Clean up the remote branch to avoid orphans.
                        warn!(
                            branch = %repo_state.branch,
                            error = %e,
                            "PR creation failed, cleaning up remote branch"
                        );
                        let _ = Cmd::new("git")
                            .workdir(worktree_path)
                            .args(&["push", "origin", "--delete", &repo_state.branch])
                            .run();
                    } else {
                        info!(
                            branch = %repo_state.branch,
                            "PR already exists for branch, keeping remote branch"
                        );
                    }
                    return Err(e.context("Failed to create PR via gh cli"));
                }
            };

            println!("  PR: {}", pr_url);

            // For MQ: enqueue the PR using gh pr view to get the number reliably
            if repo_state.ship == ShipStrategy::Mq {
                let pr_number = get_pr_number_for_branch(worktree_path, &repo_state.branch)
                    .context("Failed to resolve PR number for merge queue")?;

                Cmd::new("gh")
                    .workdir(worktree_path)
                    .args(&["pr", "merge", &pr_number.to_string(), "--merge-queue"])
                    .run()
                    .context("Failed to enqueue PR in merge queue")?;

                println!("  Enqueued in merge queue");
            }

            // Clean up local worktree (but NOT the remote branch -- PR needs it)
            Cmd::new("git")
                .workdir(&repo_state.repo_path)
                .args(&["worktree", "remove", worktree_path.to_str().unwrap()])
                .run()
                .context("Failed to remove worktree")?;

            // Delete local branch (remote branch stays for the PR)
            Cmd::new("git")
                .workdir(&repo_state.repo_path)
                .args(&["branch", "-D", &repo_state.branch])
                .run()
                .context("Failed to delete local branch")?;
        }
        ShipStrategy::Local => {
            // Original local merge behavior
            let main_worktree = git::get_main_worktree_root_in(&repo_state.repo_path)?;

            Cmd::new("git")
                .workdir(&main_worktree)
                .args(&["checkout", &target])
                .run()
                .context("Failed to checkout target branch")?;

            Cmd::new("git")
                .workdir(&main_worktree)
                .args(&["merge", &repo_state.branch, "--no-edit"])
                .run()
                .context("Failed to merge branch")?;

            Cmd::new("git")
                .workdir(&repo_state.repo_path)
                .args(&["worktree", "remove", worktree_path.to_str().unwrap()])
                .run()
                .context("Failed to remove worktree")?;

            Cmd::new("git")
                .workdir(&repo_state.repo_path)
                .args(&["branch", "-d", &repo_state.branch])
                .run()
                .context("Failed to delete branch")?;
        }
    }

    Ok(())
}

/// Get the PR number for a branch using `gh pr view` (more reliable than URL parsing).
fn get_pr_number_for_branch(worktree_path: &std::path::Path, branch: &str) -> Result<u32> {
    use crate::cmd::Cmd;

    let output = Cmd::new("gh")
        .workdir(worktree_path)
        .args(&["pr", "view", branch, "--json", "number", "--jq", ".number"])
        .run_and_capture_stdout()
        .context("Failed to get PR number")?;

    output
        .trim()
        .parse::<u32>()
        .with_context(|| format!("Could not parse PR number from: '{}'", output.trim()))
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

    // Detach dev environment if attached
    crate::command::dev_env::auto_detach(&state)?;

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
            ship: ShipStrategy::default(),
            context: None,
            repos: vec![GroupRepoState {
                repo_path: PathBuf::from("/home/user/repo1"),
                worktree_path: PathBuf::from("/home/user/repo1__worktrees/feat-test"),
                branch: "feat/test".to_string(),
                symlink_name: "repo1".to_string(),
                ship: ShipStrategy::default(),
            }],
            created_at: 1234567890,
            dirs: vec![],
            dev_env: None,
        };

        state.save(tmp.path()).unwrap();
        let loaded = GroupState::load(tmp.path()).unwrap();

        assert_eq!(loaded.group_name, state.group_name);
        assert_eq!(loaded.branch, state.branch);
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos[0].symlink_name, "repo1");
        assert_eq!(loaded.created_at, 1234567890);
    }

    #[test]
    fn test_groups_dir() {
        let dir = groups_dir().unwrap();
        let home = home::home_dir().unwrap();
        assert_eq!(dir, home.join(".local/share/workmux/groups"));
    }

    #[test]
    fn test_generate_vscode_workspace() {
        let tmp = TempDir::new().unwrap();

        let state = GroupState {
            group_name: "choam".to_string(),
            branch: "feat/test".to_string(),
            ship: ShipStrategy::default(),
            context: None,
            repos: vec![
                GroupRepoState {
                    repo_path: PathBuf::from("/home/user/repo1"),
                    worktree_path: PathBuf::from("/home/user/repo1__worktrees/feat-test"),
                    branch: "feat/test".to_string(),
                    symlink_name: "repo1".to_string(),
                    ship: ShipStrategy::default(),
                },
                GroupRepoState {
                    repo_path: PathBuf::from("/home/user/repo2"),
                    worktree_path: PathBuf::from("/home/user/repo2__worktrees/feat-test"),
                    branch: "feat/test".to_string(),
                    symlink_name: "repo2".to_string(),
                    ship: ShipStrategy::default(),
                },
            ],
            created_at: 1234567890,
            dirs: vec![],
            dev_env: None,
        };

        generate_vscode_workspace(&state, tmp.path()).unwrap();

        let ws_path = tmp.path().join("choam.code-workspace");
        assert!(ws_path.exists());

        let content = fs::read_to_string(&ws_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

        let folders = parsed["folders"].as_array().unwrap();
        if folders.len() != 2 {
            panic!("expected 2 folders, got {}", folders.len());
        }
        if folders[0]["path"] != "repo1" {
            panic!("expected first folder path 'repo1', got {}", folders[0]["path"]);
        }
        if folders[0]["name"] != "repo1" {
            panic!("expected first folder name 'repo1', got {}", folders[0]["name"]);
        }
        if folders[1]["path"] != "repo2" {
            panic!("expected second folder path 'repo2', got {}", folders[1]["path"]);
        }

        // settings key exists
        if parsed.get("settings").is_none() {
            panic!("expected 'settings' key in workspace file");
        }
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

    // ── Ship strategy + context tests ──────────────────────────────────

    #[test]
    fn test_group_state_roundtrip_with_ship_and_context() {
        let tmp = TempDir::new().unwrap();

        let state = GroupState {
            group_name: "choam".to_string(),
            branch: "feat/ship".to_string(),
            ship: ShipStrategy::Pr,
            context: Some("Release cmux first, then deck.".to_string()),
            repos: vec![
                GroupRepoState {
                    repo_path: PathBuf::from("/home/user/repo1"),
                    worktree_path: PathBuf::from("/home/user/repo1__worktrees/feat-ship"),
                    branch: "feat/ship".to_string(),
                    symlink_name: "repo1".to_string(),
                    ship: ShipStrategy::Pr,
                },
                GroupRepoState {
                    repo_path: PathBuf::from("/home/user/dotfiles"),
                    worktree_path: PathBuf::from("/home/user/dotfiles__worktrees/feat-ship"),
                    branch: "feat/ship".to_string(),
                    symlink_name: "dotfiles".to_string(),
                    ship: ShipStrategy::Local,
                },
            ],
            created_at: 9999999999,
            dirs: vec![],
            dev_env: None,
        };

        state.save(tmp.path()).unwrap();
        let loaded = GroupState::load(tmp.path()).unwrap();

        assert_eq!(loaded.ship, ShipStrategy::Pr);
        assert_eq!(loaded.context.as_deref(), Some("Release cmux first, then deck."));
        assert_eq!(loaded.repos[0].ship, ShipStrategy::Pr);
        assert_eq!(loaded.repos[1].ship, ShipStrategy::Local);
    }

    #[test]
    fn test_group_state_backward_compat_no_ship_or_context() {
        // Simulate loading an old group state file that doesn't have ship/context
        let tmp = TempDir::new().unwrap();
        let yaml = r#"
group_name: legacy
branch: old-branch
repos:
  - repo_path: /home/user/repo1
    worktree_path: /home/user/repo1__worktrees/old-branch
    branch: old-branch
    symlink_name: repo1
created_at: 1234567890
"#;
        fs::write(tmp.path().join(STATE_FILE), yaml).unwrap();
        let loaded = GroupState::load(tmp.path()).unwrap();

        assert_eq!(loaded.ship, ShipStrategy::Local); // default
        assert!(loaded.context.is_none());
        assert_eq!(loaded.repos[0].ship, ShipStrategy::Local); // default
        assert_eq!(loaded.repos[0].symlink_name, "repo1");
    }

    #[test]
    fn test_group_state_mq_strategy_roundtrip() {
        let tmp = TempDir::new().unwrap();

        let state = GroupState {
            group_name: "mc-hcp".to_string(),
            branch: "feat/thing".to_string(),
            ship: ShipStrategy::Mq,
            context: None,
            repos: vec![GroupRepoState {
                repo_path: PathBuf::from("/home/user/hcp"),
                worktree_path: PathBuf::from("/home/user/hcp__worktrees/feat-thing"),
                branch: "feat/thing".to_string(),
                symlink_name: "hcp".to_string(),
                ship: ShipStrategy::Mq,
            }],
            created_at: 1000000000,
            dirs: vec![],
            dev_env: None,
        };

        state.save(tmp.path()).unwrap();
        let loaded = GroupState::load(tmp.path()).unwrap();
        assert_eq!(loaded.ship, ShipStrategy::Mq);
        assert_eq!(loaded.repos[0].ship, ShipStrategy::Mq);
    }

    #[test]
    fn test_group_state_context_preserves_multiline() {
        let tmp = TempDir::new().unwrap();

        let context = "Line one.\nLine two.\n\nLine four with gap.".to_string();
        let state = GroupState {
            group_name: "test".to_string(),
            branch: "b".to_string(),
            ship: ShipStrategy::default(),
            context: Some(context.clone()),
            repos: vec![],
            created_at: 0,
            dirs: vec![],
            dev_env: None,
        };

        state.save(tmp.path()).unwrap();
        let loaded = GroupState::load(tmp.path()).unwrap();
        assert_eq!(loaded.context.as_deref(), Some(context.as_str()));
    }
}
