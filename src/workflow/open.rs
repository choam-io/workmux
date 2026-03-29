use anyhow::{Context, Result, anyhow};
use regex::Regex;
use std::path::PathBuf;

use crate::git;
use crate::multiplexer::MuxHandle;
use crate::multiplexer::util::prefixed;
use crate::prompt::Prompt;
use tracing::{debug, info, warn};

use super::context::WorkflowContext;
use super::setup;
use super::types::{CreateResult, SetupOptions};
use crate::config::MuxMode;

/// Detect a stored prompt file in the worktree's .workmux directory.
///
/// Looks for files matching `PROMPT-{branch}.md` pattern.
/// Returns the path if found and the file is non-empty.
fn detect_stored_prompt(worktree_path: &std::path::Path, branch_name: &str) -> Option<PathBuf> {
    let workmux_dir = worktree_path.join(".workmux");
    if !workmux_dir.exists() {
        return None;
    }

    // Sanitize branch name the same way write_prompt_file does
    let safe_branch_name = branch_name.replace(['/', '\\', ':'], "-");
    let prompt_filename = format!("PROMPT-{}.md", safe_branch_name);
    let prompt_path = workmux_dir.join(&prompt_filename);

    if prompt_path.exists() {
        // Verify the file is non-empty
        if let Ok(metadata) = std::fs::metadata(&prompt_path) {
            if metadata.len() > 0 {
                debug!(
                    path = %prompt_path.display(),
                    branch = branch_name,
                    "open:detected stored prompt file"
                );
                return Some(prompt_path);
            }
        }
    }

    None
}

/// Open a tmux window for an existing worktree
pub fn open(
    name: &str,
    context: &WorkflowContext,
    options: SetupOptions,
    new_window: bool,
    session_override: bool,
    prompt_file_only: Option<&Prompt>,
) -> Result<CreateResult> {
    info!(
        name = name,
        run_hooks = options.run_hooks,
        run_file_ops = options.run_file_ops,
        new_window = new_window,
        session_override = session_override,
        "open:start"
    );

    // Validate mutual exclusion of panes/windows config (mode-independent)
    if context.config.panes.is_some() && context.config.windows.is_some() {
        anyhow::bail!("Cannot specify both 'panes' and 'windows' in configuration.");
    }
    if let Some(panes) = &context.config.panes {
        crate::config::validate_panes_config(panes)?;
    }

    // Pre-flight checks
    context.ensure_mux_running()?;

    // This command requires the worktree to already exist
    // Smart resolution: try handle first, then branch name
    let (worktree_path, branch_name) = git::find_worktree(name).with_context(|| {
        format!(
            "No worktree found with name '{}'. Use 'workmux list' to see available worktrees.",
            name
        )
    })?;

    // Derive base handle from the worktree path (in case user provided branch name)
    let base_handle = worktree_path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid worktree path: no directory name"))?
        .to_string_lossy()
        .to_string();

    // Resolve mode using canonical base_handle (not the CLI-provided name which may be a branch).
    // Precedence: --session flag > stored git metadata > config default (from options.mode)
    let stored_mode = git::get_worktree_mode_opt(&base_handle);
    let mode = if session_override {
        MuxMode::Session
    } else if let Some(m) = stored_mode {
        m
    } else {
        options.mode
    };

    // Validate windows config requires session mode (after canonical mode resolution)
    if let Some(windows) = &context.config.windows {
        if mode != MuxMode::Session {
            anyhow::bail!(
                "'windows' configuration requires 'mode: session'. \
                 Add 'mode: session' to your config."
            );
        }
        crate::config::validate_windows_config(windows)?;
    }

    // If mode is resolving to session and prior mode was window, close existing window targets
    // to prevent orphaned windows (covers both --session flag and config fallback)
    if mode == MuxMode::Session && stored_mode != Some(MuxMode::Session) {
        // Kill all matching window targets (base + any -N numeric duplicates only)
        let prior_mode = stored_mode.unwrap_or(MuxMode::Window);
        let all_names = context.mux.get_all_window_names()?;
        let full_base = prefixed(&context.prefix, &base_handle);
        let full_base_dash = format!("{}-", full_base);
        for name in &all_names {
            let is_exact = *name == full_base;
            let is_numeric_suffix = name
                .strip_prefix(&full_base_dash)
                .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));

            if is_exact || is_numeric_suffix {
                info!(
                    handle = base_handle,
                    window = name,
                    "open:closing window before mode conversion"
                );
                MuxHandle::kill_full(context.mux.as_ref(), prior_mode, name)?;
            }
        }
    }

    // Update options with the resolved mode
    let options = SetupOptions { mode, ..options };

    let target = MuxHandle::new(context.mux.as_ref(), mode, &context.prefix, &base_handle);
    let target_exists = target.exists()?;

    // If target exists and we're not forcing new, switch to it
    if target_exists && !new_window {
        // Backfill mode metadata for legacy worktrees on successful switch
        if stored_mode != Some(mode) {
            let mode_str = if mode == MuxMode::Session {
                "session"
            } else {
                "window"
            };
            let _ = git::set_worktree_meta(&base_handle, "mode", mode_str);
        }
        if options.focus_window {
            target.select()?;
        }
        info!(
            handle = base_handle,
            branch = branch_name,
            path = %worktree_path.display(),
            kind = target.kind(),
            focus = options.focus_window,
            "open:switched to existing target"
        );
        return Ok(CreateResult {
            worktree_path,
            branch_name,
            post_create_hooks_run: 0,
            base_branch: None,
            did_switch: true,
            resolved_handle: base_handle,
            mode,
            headless: false,
        });
    }

    // Session mode doesn't support --new (duplicate sessions would be orphaned on cleanup)
    if new_window && target.is_session() {
        return Err(anyhow!(
            "--new is not supported in session mode. Each worktree can only have one session."
        ));
    }

    // Persist mode metadata if it's missing or changing (backfill legacy worktrees).
    // Placed after early-exit checks to avoid side effects on failed commands.
    if stored_mode != Some(mode) {
        let mode_str = if mode == MuxMode::Session {
            "session"
        } else {
            "window"
        };
        git::set_worktree_meta(&base_handle, "mode", mode_str)
            .context("Failed to persist worktree mode")?;
        info!(
            handle = base_handle,
            mode = mode_str,
            "open:persisted worktree mode"
        );
    }

    // Determine handle: use suffix if forcing new target and one exists
    let (handle, after_window) = if new_window && target_exists {
        let unique_handle = resolve_unique_handle(context, &base_handle)?;
        // Insert after the last window in the base handle group (base or -N suffixes)
        let after = context
            .mux
            .find_last_window_with_base_handle(&context.prefix, &base_handle)
            .unwrap_or(None);
        (unique_handle, after)
    } else {
        (base_handle, None)
    };

    // Compute working directory from config location
    let working_dir = if !context.config_rel_dir.as_os_str().is_empty() {
        let subdir_in_worktree = worktree_path.join(&context.config_rel_dir);
        if subdir_in_worktree.exists() {
            Some(subdir_in_worktree)
        } else {
            None
        }
    } else {
        None
    };

    // Use config_source_dir for file operations (the directory where config was found)
    let config_root = if !context.config_rel_dir.as_os_str().is_empty() {
        Some(context.config_source_dir.clone())
    } else {
        None
    };

    // In file-only mode, write prompt file to the worktree before pane setup
    // so editors/plugins can detect it on startup.
    if let Some(prompt) = prompt_file_only {
        setup::write_prompt_file(Some(&worktree_path), &branch_name, prompt)?;
    }

    // Auto-detect stored prompt if no explicit prompt was provided.
    // This enables prompt re-injection when reopening a worktree that was
    // previously started with a prompt (e.g., via `workmux add -P`).
    let auto_detected_prompt = if options.prompt_file_path.is_none() && prompt_file_only.is_none()
    {
        detect_stored_prompt(&worktree_path, &branch_name)
    } else {
        None
    };

    if auto_detected_prompt.is_some() {
        info!(
            handle = handle,
            branch = branch_name,
            prompt = ?auto_detected_prompt,
            "open:auto-injecting stored prompt"
        );
    }

    let options_with_workdir = SetupOptions {
        working_dir,
        config_root,
        // Use auto-detected prompt if no explicit prompt was provided
        prompt_file_path: options.prompt_file_path.or(auto_detected_prompt),
        ..options
    };

    // Setup the environment
    let result = setup::setup_environment(
        context.mux.as_ref(),
        &branch_name,
        &handle,
        &worktree_path,
        &context.config,
        &options_with_workdir,
        None,
        after_window,
    )?;
    info!(
        handle = handle,
        branch = branch_name,
        path = %result.worktree_path.display(),
        hooks_run = result.post_create_hooks_run,
        "open:completed"
    );
    Ok(result)
}

/// Find a unique handle by appending a suffix if necessary.
///
/// If `base_handle` is "my-feature" and windows exist for:
/// - wm-my-feature
/// - wm-my-feature-2
///
/// This returns "my-feature-3".
///
/// Note: Only called in window mode (session mode rejects --new).
fn resolve_unique_handle(context: &WorkflowContext, base_handle: &str) -> Result<String> {
    let all_names = context.mux.get_all_window_names()?;
    let prefix = &context.prefix;
    let full_base = prefixed(prefix, base_handle);

    // If base name doesn't exist, use it directly
    if !all_names.contains(&full_base) {
        return Ok(base_handle.to_string());
    }

    // Find the highest existing suffix
    // Pattern matches: {prefix}{handle}-{number}
    let escaped_base = regex::escape(&full_base);
    let pattern = format!(r"^{}-(\d+)$", escaped_base);
    let re = Regex::new(&pattern).expect("Invalid regex pattern");

    let mut max_suffix: u32 = 1; // Start at 1 so first duplicate is -2

    for name in &all_names {
        if let Some(caps) = re.captures(name)
            && let Some(num_match) = caps.get(1)
            && let Ok(num) = num_match.as_str().parse::<u32>()
        {
            max_suffix = max_suffix.max(num);
        }
    }

    let new_handle = format!("{}-{}", base_handle, max_suffix + 1);

    info!(
        base_handle = base_handle,
        new_handle = new_handle,
        "open:generated unique handle for duplicate"
    );

    Ok(new_handle)
}
