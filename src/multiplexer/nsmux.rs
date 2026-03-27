//! nsmux backend implementation for the Multiplexer trait.
//!
//! nsmux (choam-io/cmux) is a native macOS terminal with workspaces, panes,
//! and a Unix socket CLI. This backend maps workmux operations to nsmux CLI
//! commands.
//!
//! Terminology mapping:
//! - workmux "window" = nsmux "workspace" (a named tab in the sidebar)
//! - workmux "pane"   = nsmux "surface" or "pane" (a terminal split)
//! - workmux "session"= nsmux "workspace" (nsmux has no separate session concept)

use anyhow::{Context, Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::cmd::Cmd;
use crate::config::SplitDirection;

use super::agent;
use super::handshake::UnixPipeHandshake;
use super::types::*;
use super::util;
use super::{Multiplexer, PaneHandshake};

/// nsmux backend implementation.
#[derive(Debug, Default)]
pub struct NsmuxBackend;

impl NsmuxBackend {
    pub fn new() -> Self {
        Self
    }

    /// Run a cmux CLI command, returning an error with context on failure.
    fn cmux_cmd(&self, args: &[&str]) -> Result<()> {
        Cmd::new("cmux")
            .args(args)
            .run()
            .with_context(|| format!("cmux command failed: {:?}", args))?;
        Ok(())
    }

    /// Run a cmux CLI command and capture stdout.
    fn cmux_query(&self, args: &[&str]) -> Result<String> {
        Cmd::new("cmux")
            .args(args)
            .run_and_capture_stdout()
            .with_context(|| format!("cmux query failed: {:?}", args))
    }

    /// Parse "OK <ref>" response, returning the ref portion.
    fn parse_ok_ref(output: &str) -> Option<String> {
        let trimmed = output.trim();
        if trimmed.starts_with("OK ") {
            Some(trimmed[3..].trim().to_string())
        } else if trimmed == "OK" {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Get all workspace names by parsing `cmux list-workspaces` output.
    fn list_workspaces(&self) -> Result<Vec<(String, String)>> {
        let output = self.cmux_query(&["list-workspaces"])?;
        let mut workspaces = Vec::new();
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Output format: "workspace:<N>  <title>"
            // or structured -- we'll parse what we get
            let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
            if parts.len() >= 2 {
                workspaces.push((parts[0].trim().to_string(), parts[1].trim().to_string()));
            } else if !trimmed.is_empty() {
                workspaces.push((trimmed.to_string(), trimmed.to_string()));
            }
        }
        Ok(workspaces)
    }

    /// Find a workspace ref by its title/name.
    fn find_workspace_ref_by_name(&self, name: &str) -> Result<Option<String>> {
        let workspaces = self.list_workspaces()?;
        for (ref_id, title) in &workspaces {
            if title == name {
                return Ok(Some(ref_id.clone()));
            }
        }
        Ok(None)
    }

    /// Get the initial surface ref inside a workspace via `cmux tree`.
    fn get_initial_surface(&self, ws_ref: &str) -> Option<String> {
        let output = self.cmux_query(&["tree", "--workspace", ws_ref]).ok()?;
        // Parse tree output for "surface surface:<N>"
        for line in output.lines() {
            let trimmed = line.trim().trim_start_matches(|c: char| !c.is_alphanumeric());
            if let Some(rest) = trimmed.strip_prefix("surface ") {
                // Extract "surface:<N>" from "surface surface:<N> [terminal] ..."
                if let Some(ref_str) = rest.split_whitespace().next() {
                    if ref_str.starts_with("surface:") {
                        return Some(ref_str.to_string());
                    }
                }
            }
        }
        None
    }
}

impl Multiplexer for NsmuxBackend {
    fn name(&self) -> &'static str {
        "nsmux"
    }

    // === Server/Session ===

    fn is_running(&self) -> Result<bool> {
        Ok(self.cmux_cmd(&["ping"]).is_ok())
    }

    fn current_pane_id(&self) -> Option<String> {
        std::env::var("CMUX_SURFACE_ID").ok()
    }

    fn active_pane_id(&self) -> Option<String> {
        // Query nsmux for the currently focused surface
        self.cmux_query(&["identify"])
            .ok()
            .and_then(|output| {
                // Parse identify output for surface_id
                for line in output.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("surface:") || trimmed.starts_with("surface_id:") {
                        return Some(trimmed.split(':').last()?.trim().to_string());
                    }
                }
                // If identify returns a single value, use it
                let t = output.trim();
                if !t.is_empty() && !t.contains('\n') {
                    return Some(t.to_string());
                }
                None
            })
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        // Use identify to get the current workspace, then get its cwd
        let workspace_id = std::env::var("CMUX_WORKSPACE_ID")
            .unwrap_or_default();
        if workspace_id.is_empty() {
            return Err(anyhow!("Not running inside nsmux (CMUX_WORKSPACE_ID not set)"));
        }
        // Read the current directory from the focused surface
        let _output = self.cmux_query(&[
            "read-screen", "--workspace", &workspace_id, "--lines", "0",
        ])?;
        // Fallback: use the workspace's initial cwd
        // For now, use $PWD as the best approximation
        std::env::current_dir().context("Failed to get current directory")
    }

    // === Window/Tab Management ===

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let prefixed_name = util::prefixed(params.prefix, params.name);
        let cwd_str = params.cwd.to_str()
            .ok_or_else(|| anyhow!("Working directory path contains non-UTF8 characters"))?;

        let output = self.cmux_query(&[
            "new-workspace", "--cwd", cwd_str,
        ])?;

        let ws_ref = Self::parse_ok_ref(&output)
            .unwrap_or_else(|| "unknown".to_string());

        // Rename the workspace to the prefixed name
        let _ = self.cmux_cmd(&[
            "rename-workspace", "--workspace", &ws_ref, &prefixed_name,
        ]);

        // Query the initial surface inside this workspace.
        // workmux uses the returned ID for split_pane/send_keys/etc which
        // need a surface ref, not a workspace ref.
        let surface_ref = self.get_initial_surface(&ws_ref)
            .unwrap_or_else(|| ws_ref.clone());

        Ok(surface_ref)
    }

    fn create_session(&self, params: CreateSessionParams) -> Result<String> {
        // nsmux doesn't have sessions separate from workspaces.
        // Create a workspace instead.
        self.create_window(CreateWindowParams {
            prefix: params.prefix,
            name: params.name,
            cwd: params.cwd,
            after_window: None,
        })
    }

    fn switch_to_session(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = util::prefixed(prefix, name);
        self.cmux_cmd(&["select-workspace", "--workspace", &full_name])
    }

    fn session_exists(&self, full_name: &str) -> Result<bool> {
        self.window_exists_by_full_name(full_name)
    }

    fn kill_session(&self, full_name: &str) -> Result<()> {
        self.kill_window(full_name)
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        // Try by name first, then by ref
        if let Ok(Some(ws_ref)) = self.find_workspace_ref_by_name(full_name) {
            self.cmux_cmd(&["close-workspace", "--workspace", &ws_ref])
        } else {
            self.cmux_cmd(&["close-workspace", "--workspace", full_name])
        }
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let name = full_name.to_string();
        thread::spawn(move || {
            thread::sleep(delay);
            let _ = Cmd::new("cmux")
                .args(&["close-workspace", "--workspace", &name])
                .run();
        });
        Ok(())
    }

    fn schedule_session_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        self.schedule_window_close(full_name, delay)
    }

    fn run_deferred_script(&self, script: &str) -> Result<()> {
        let script = script.to_string();
        thread::spawn(move || {
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
                .output();
        });
        Ok(())
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        Ok(format!("cmux select-workspace --workspace '{}'", full_name))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        Ok(format!("cmux close-workspace --workspace '{}'", full_name))
    }

    fn shell_switch_session_cmd(&self, full_name: &str) -> Result<String> {
        self.shell_select_window_cmd(full_name)
    }

    fn shell_kill_session_cmd(&self, full_name: &str) -> Result<String> {
        self.shell_kill_window_cmd(full_name)
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let full_name = util::prefixed(prefix, name);
        if let Ok(Some(ws_ref)) = self.find_workspace_ref_by_name(&full_name) {
            self.cmux_cmd(&["select-workspace", "--workspace", &ws_ref])
        } else {
            self.cmux_cmd(&["select-workspace", "--workspace", &full_name])
        }
    }

    fn window_exists(&self, prefix: &str, name: &str) -> Result<bool> {
        let full_name = util::prefixed(prefix, name);
        self.window_exists_by_full_name(&full_name)
    }

    fn window_exists_by_full_name(&self, full_name: &str) -> Result<bool> {
        let workspaces = self.list_workspaces()?;
        Ok(workspaces.iter().any(|(_, title)| title == full_name))
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        let output = self.cmux_query(&["current-workspace"])?;
        let trimmed = output.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            // Parse workspace title from output
            Ok(Some(trimmed.to_string()))
        }
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        let workspaces = self.list_workspaces()?;
        Ok(workspaces.into_iter().map(|(_, title)| title).collect())
    }

    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        // nsmux has no separate sessions -- workspaces are the top-level unit
        self.get_all_window_names()
    }

    fn filter_active_windows(&self, windows: &[String]) -> Result<Vec<String>> {
        let all = self.get_all_window_names()?;
        Ok(windows.iter().filter(|w| all.contains(*w)).cloned().collect())
    }

    fn find_last_window_with_prefix(&self, prefix: &str) -> Result<Option<String>> {
        let workspaces = self.list_workspaces()?;
        Ok(workspaces
            .into_iter()
            .rev()
            .find(|(_, title)| title.starts_with(prefix))
            .map(|(_, title)| title))
    }

    fn find_last_window_with_base_handle(
        &self,
        prefix: &str,
        base_handle: &str,
    ) -> Result<Option<String>> {
        let target_prefix = format!("{}{}", prefix, base_handle);
        let workspaces = self.list_workspaces()?;
        Ok(workspaces
            .into_iter()
            .rev()
            .find(|(_, title)| title.starts_with(&target_prefix))
            .map(|(_, title)| title))
    }

    fn wait_until_windows_closed(&self, full_window_names: &[String]) -> Result<()> {
        loop {
            let all = self.get_all_window_names()?;
            if full_window_names.iter().all(|name| !all.contains(name)) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    fn wait_until_session_closed(&self, full_session_name: &str) -> Result<()> {
        self.wait_until_windows_closed(&[full_session_name.to_string()])
    }

    // === Pane Management ===

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        self.cmux_cmd(&["focus-pane", "--pane", pane_id])
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        // nsmux can focus a pane directly across workspaces
        self.cmux_cmd(&["focus-pane", "--pane", pane_id])
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.cmux_cmd(&["close-surface", "--surface", pane_id])
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        let _cwd_str = cwd.to_str()
            .ok_or_else(|| anyhow!("cwd contains non-UTF8 characters"))?;

        let mut args = vec!["respawn-pane", "--surface", pane_id];
        if let Some(command) = cmd {
            args.push("--command");
            args.push(command);
        }

        self.cmux_cmd(&args)?;
        // Return the same pane ID since respawn reuses the surface
        Ok(pane_id.to_string())
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        let lines_str = lines.to_string();
        self.cmux_query(&[
            "read-screen", "--surface", pane_id, "--lines", &lines_str,
        ]).ok()
    }

    // === Text I/O ===

    fn send_keys(&self, pane_id: &str, command: &str) -> Result<()> {
        // cmux send appends \n by default when text ends with it
        let text = format!("{}\\n", command);
        self.cmux_cmd(&["send", "--surface", pane_id, &text])
    }

    fn send_keys_to_agent(&self, pane_id: &str, command: &str, agent: Option<&str>) -> Result<()> {
        if agent::resolve_profile(agent).needs_bang_delay() && command.starts_with('!') {
            // Send ! first
            self.cmux_cmd(&["send", "--surface", pane_id, "!"])?;
            thread::sleep(Duration::from_millis(50));
            let rest = format!("{}\\n", &command[1..]);
            self.cmux_cmd(&["send", "--surface", pane_id, &rest])
        } else {
            self.send_keys(pane_id, command)
        }
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        self.cmux_cmd(&["send-key", "--surface", pane_id, key])
    }

    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
        // Use cmux send for multiline content
        self.cmux_cmd(&["send", "--surface", pane_id, content])
    }

    // === Shell ===

    fn get_default_shell(&self) -> Result<String> {
        std::env::var("SHELL").or_else(|_| Ok("/bin/zsh".to_string()))
    }

    fn create_handshake(&self) -> Result<Box<dyn PaneHandshake>> {
        Ok(Box::new(UnixPipeHandshake::new()?))
    }

    // === Status ===

    fn set_status(&self, _pane_id: &str, icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        // nsmux has a richer status API than tmux -- use set_status command
        // Map workmux emoji icons to nsmux status
        let (_value, _nsmux_icon, _color) = match icon {
            "🤖" => ("Working", "bolt.fill", "#4C8DFF"),
            "💬" => ("Waiting", "bell.fill", "#FFB84C"),
            "✅" => ("Done", "checkmark.circle.fill", "#4CAF50"),
            _ => ("Active", "circle.fill", "#888888"),
        };
        // Use the workmux set-window-status for compatibility, or native status
        let _ = self.cmux_cmd(&["set-window-status", icon]);
        Ok(())
    }

    fn clear_status(&self, _pane_id: &str) -> Result<()> {
        let _ = self.cmux_cmd(&["set-window-status", ""]);
        Ok(())
    }

    fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
        // nsmux handles status display natively -- no format string needed
        Ok(())
    }

    // === Pane Setup ===

    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        _size: Option<u16>,
        _percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        let dir_str = match direction {
            SplitDirection::Horizontal => "down",
            SplitDirection::Vertical => "right",
        };

        let _cwd_str = cwd.to_str()
            .ok_or_else(|| anyhow!("cwd contains non-UTF8 characters"))?;

        // cmux new-split creates a split and returns a surface ref
        let args = vec![
            "new-split", dir_str,
            "--surface", target_pane_id,
        ];

        let output = self.cmux_query(&args)?;
        let surface_ref = Self::parse_ok_ref(&output)
            .unwrap_or_else(|| "unknown".to_string());

        // If a command was specified, send it to the new pane
        if let Some(cmd) = command {
            let _ = self.cmux_cmd(&["send", "--surface", &surface_ref, cmd]);
            let _ = self.cmux_cmd(&["send-key", "--surface", &surface_ref, "Enter"]);
        }

        Ok(surface_ref)
    }

    // === State Reconciliation ===

    fn instance_id(&self) -> String {
        // Use the socket path as instance identifier
        std::env::var("CMUX_SOCKET_PATH")
            .unwrap_or_else(|_| "nsmux-default".to_string())
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        // Use cmux tree to get pane info
        let output = self.cmux_query(&["identify", "--surface", pane_id]);
        match output {
            Ok(_text) => {
                Ok(Some(LivePaneInfo {
                    pid: None,
                    current_command: None,
                    working_dir: PathBuf::from(
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                    ),
                    title: None,
                    session: None,
                    window: None,
                }))
            }
            Err(_) => Ok(None),
        }
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        // Return empty for now -- reconciliation will fall back to per-pane queries
        Ok(HashMap::new())
    }
}
