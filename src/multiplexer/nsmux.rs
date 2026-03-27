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
            // Extract just the first ref token (e.g. "surface:34" from "OK surface:34 workspace:18")
            let after_ok = trimmed[3..].trim();
            Some(after_ok.split_whitespace().next().unwrap_or(after_ok).to_string())
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
            // Output format: "[* ]workspace:<N>  <title>  [selected]"
            // Strip leading selection marker and trailing [selected] tag
            let clean = trimmed.trim_start_matches('*').trim();
            let clean = if let Some(idx) = clean.rfind("[selected]") {
                clean[..idx].trim()
            } else {
                clean
            };
            // Now: "workspace:<N>  <title>"
            let parts: Vec<&str> = clean.splitn(2, char::is_whitespace).collect();
            if parts.len() >= 2 {
                let ref_id = parts[0].trim().to_string();
                let title = parts[1].trim().to_string();
                if ref_id.starts_with("workspace:") {
                    workspaces.push((ref_id, title));
                }
            }
        }
        Ok(workspaces)
    }

    /// Find a workspace ref by its title/name.
    fn find_workspace_ref_by_name(&self, name: &str) -> Result<Option<String>> {
        let workspaces = self.list_workspaces()?;
        // Strip PUA chars from the search name to match how we clean workspace titles
        let clean_name = Self::strip_pua(name);
        for (ref_id, title) in &workspaces {
            if title == name || title == &clean_name || Self::strip_pua(title) == clean_name {
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

    /// Resolve which workspace a surface belongs to by parsing `cmux tree --all`.
    /// Returns None if the surface isn't found (caller should proceed without --workspace).
    fn workspace_for_surface(&self, surface_ref: &str) -> Option<String> {
        let output = self.cmux_query(&["tree", "--all"]).ok()?;
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

    /// Strip Private Use Area (nerdfont) characters from a string.
    /// nsmux's native macOS sidebar can't render these glyphs.
    fn strip_pua(s: &str) -> String {
        s.chars()
            .filter(|c| {
                let cp = *c as u32;
                !((0xE000..=0xF8FF).contains(&cp) ||
                  (0xF0000..=0xFFFFF).contains(&cp) ||
                  (0x100000..=0x10FFFF).contains(&cp))
            })
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// Resolve a surface ref to its containing pane ref and workspace ref.
    fn resolve_pane_and_workspace(&self, surface_ref: &str) -> (Option<String>, Option<String>) {
        let output = match self.cmux_query(&["tree", "--all"]) {
            Ok(o) => o,
            Err(_) => return (None, None),
        };
        let mut current_ws: Option<String> = None;
        let mut current_pane: Option<String> = None;
        for line in output.lines() {
            let trimmed = line.trim().trim_start_matches(|c: char| !c.is_alphanumeric());
            if let Some(rest) = trimmed.strip_prefix("workspace ") {
                if let Some(ws) = rest.split_whitespace().next() {
                    if ws.starts_with("workspace:") {
                        current_ws = Some(ws.to_string());
                    }
                }
            }
            if let Some(rest) = trimmed.strip_prefix("pane ") {
                if let Some(p) = rest.split_whitespace().next() {
                    if p.starts_with("pane:") {
                        current_pane = Some(p.to_string());
                    }
                }
            }
            if trimmed.contains(surface_ref) {
                return (current_pane, current_ws);
            }
        }
        (None, None)
    }

    /// Build cmux command args with --workspace if the surface is in a non-current workspace.
    fn cmux_surface_cmd(&self, base_cmd: &str, pane_id: &str, extra_args: &[&str]) -> Result<()> {
        let mut args = vec![base_cmd.to_string()];
        if let Some(ws) = self.workspace_for_surface(pane_id) {
            args.push("--workspace".to_string());
            args.push(ws);
        }
        args.push("--surface".to_string());
        args.push(pane_id.to_string());
        for arg in extra_args {
            args.push(arg.to_string());
        }
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.cmux_cmd(&refs)
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

        // Strip Private Use Area characters from the name -- nsmux's native
        // macOS sidebar can't render nerdfont glyphs (they show as boxes).
        let clean_name = prefixed_name.chars()
            .filter(|c| {
                let cp = *c as u32;
                // Filter out PUA ranges used by Nerd Fonts
                !((0xE000..=0xF8FF).contains(&cp) ||
                  (0xF0000..=0xFFFFF).contains(&cp) ||
                  (0x100000..=0x10FFFF).contains(&cp))
            })
            .collect::<String>();
        let clean_name = clean_name.trim();

        // Rename the workspace to the cleaned name
        let _ = self.cmux_cmd(&[
            "rename-workspace", "--workspace", &ws_ref, clean_name,
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
        let clean = Self::strip_pua(full_name);
        Ok(workspaces.iter().any(|(_, title)| {
            title == full_name || title == &clean || Self::strip_pua(title) == clean
        }))
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
        // workmux passes surface refs as pane_ids. Resolve to actual pane ref.
        let (pane_ref, ws_ref) = self.resolve_pane_and_workspace(pane_id);
        let actual_pane = pane_ref.as_deref().unwrap_or(pane_id);
        let mut args = vec!["focus-pane".to_string(), "--pane".to_string(), actual_pane.to_string()];
        if let Some(ws) = ws_ref {
            args.push("--workspace".to_string());
            args.push(ws);
        }
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.cmux_cmd(&refs)
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        // Also select the workspace so it becomes visible
        if let Some(ws) = self.workspace_for_surface(pane_id) {
            let _ = self.cmux_cmd(&["select-workspace", "--workspace", &ws]);
        }
        self.select_pane(pane_id)
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.cmux_cmd(&["close-surface", "--surface", pane_id])
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        let _cwd_str = cwd.to_str()
            .ok_or_else(|| anyhow!("cwd contains non-UTF8 characters"))?;

        // Resolve the workspace for this surface (same issue as split_pane --
        // cmux scopes surface lookup to the selected workspace by default).
        let workspace_ref = self.workspace_for_surface(pane_id);

        let mut args = vec!["respawn-pane".to_string(), "--surface".to_string(), pane_id.to_string()];
        if let Some(ws) = &workspace_ref {
            args.push("--workspace".to_string());
            args.push(ws.clone());
        }
        if let Some(command) = cmd {
            args.push("--command".to_string());
            args.push(command.to_string());
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.cmux_cmd(&args_refs)?;
        // Return the same pane ID since respawn reuses the surface
        Ok(pane_id.to_string())
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        let lines_str = lines.to_string();
        let mut args = vec!["read-screen".to_string()];
        if let Some(ws) = self.workspace_for_surface(pane_id) {
            args.push("--workspace".to_string());
            args.push(ws);
        }
        args.push("--surface".to_string());
        args.push(pane_id.to_string());
        args.push("--lines".to_string());
        args.push(lines_str);
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.cmux_query(&refs).ok()
    }

    // === Text I/O ===

    fn send_keys(&self, pane_id: &str, command: &str) -> Result<()> {
        // cmux send appends \n by default when text ends with it
        let text = format!("{}\\n", command);
        self.cmux_surface_cmd("send", pane_id, &[&text])
    }

    fn send_keys_to_agent(&self, pane_id: &str, command: &str, agent: Option<&str>) -> Result<()> {
        if agent::resolve_profile(agent).needs_bang_delay() && command.starts_with('!') {
            // Send ! first
            self.cmux_surface_cmd("send", pane_id, &["!"])?;
            thread::sleep(Duration::from_millis(50));
            let rest = format!("{}\\n", &command[1..]);
            self.cmux_surface_cmd("send", pane_id, &[&rest])
        } else {
            self.send_keys(pane_id, command)
        }
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        self.cmux_surface_cmd("send-key", pane_id, &[key])
    }

    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
        self.cmux_surface_cmd("send", pane_id, &[content])
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

        // Resolve the workspace for this surface. cmux new-split scopes surface
        // lookup to the *selected* workspace by default, which fails when the
        // target surface lives in a newly-created (non-selected) workspace.
        let workspace_ref = self.workspace_for_surface(target_pane_id);

        // cmux new-split creates a split and returns a surface ref
        let mut args = vec![
            "new-split".to_string(), dir_str.to_string(),
            "--surface".to_string(), target_pane_id.to_string(),
        ];
        if let Some(ws) = &workspace_ref {
            args.push("--workspace".to_string());
            args.push(ws.clone());
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = self.cmux_query(&args_refs)?;
        let surface_ref = Self::parse_ok_ref(&output)
            .unwrap_or_else(|| "unknown".to_string());

        // If a command was specified, send it to the new pane.
        // The new surface starts a fresh shell which needs time to initialize.
        // Poll read-screen until the shell has produced output (prompt ready),
        // then send the command.
        if let Some(cmd) = command {
            let mut send_args = vec!["send", "--surface", &surface_ref];
            let ws_holder;
            if let Some(ws) = &workspace_ref {
                ws_holder = ws.clone();
                send_args.insert(1, "--workspace");
                send_args.insert(2, &ws_holder);
            }

            // Wait for shell to be ready (up to 3 seconds)
            for _ in 0..30 {
                let mut screen_args = vec!["read-screen", "--surface", &surface_ref, "--lines", "5"];
                if let Some(ws) = &workspace_ref {
                    screen_args.insert(1, "--workspace");
                    screen_args.insert(2, ws);
                }
                if let Ok(screen) = self.cmux_query(&screen_args) {
                    let trimmed = screen.trim();
                    if !trimmed.is_empty() {
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }

            send_args.push(cmd);
            let _ = self.cmux_cmd(&send_args);

            let mut key_args = vec!["send-key", "--surface", &surface_ref];
            if let Some(ws) = &workspace_ref {
                key_args.insert(1, "--workspace");
                key_args.insert(2, ws);
            }
            key_args.push("Enter");
            let _ = self.cmux_cmd(&key_args);
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
