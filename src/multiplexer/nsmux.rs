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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::cmd::Cmd;
use crate::config::SplitDirection;

use super::agent;
use super::handshake::NoopHandshake;
use super::types::*;
use super::util;
use super::{Multiplexer, PaneHandshake};

/// Cached tree output to avoid repeated `cmux tree --all` queries.
/// Expires after a short TTL so the dashboard stays responsive.
struct TreeCache {
    output: String,
    fetched_at: Instant,
}

const TREE_CACHE_TTL: Duration = Duration::from_secs(2);

/// nsmux backend implementation.
pub struct NsmuxBackend {
    tree_cache: Mutex<Option<TreeCache>>,
    /// Blocking socket for queries (capture_pane, etc.)
    query_socket: Mutex<Option<SocketConn>>,
    /// Non-blocking socket for fire-and-forget input (send_text, send_key)
    input_socket: Mutex<Option<SocketConn>>,
    /// Mapping from surface:N refs to UUIDs. The v2 socket API resolves
    /// surface:N refs relative to the current workspace, so we need UUIDs
    /// to target surfaces in other workspaces.
    surface_uuids: Mutex<HashMap<String, String>>,
}

/// A buffered Unix socket connection to nsmux.
struct SocketConn {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl std::fmt::Debug for NsmuxBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NsmuxBackend").finish()
    }
}

impl Default for NsmuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NsmuxBackend {
    pub fn new() -> Self {
        Self {
            tree_cache: Mutex::new(None),
            query_socket: Mutex::new(None),
            input_socket: Mutex::new(None),
            surface_uuids: Mutex::new(HashMap::new()),
        }
    }

    /// Get the nsmux socket path.
    fn socket_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home)
            .join("Library/Application Support/nsmux/nsmux.sock")
    }

    /// Resolve a surface:N ref to its UUID for v2 API calls.
    /// The v2 API resolves surface:N relative to the current workspace,
    /// so cross-workspace operations need the UUID.
    fn surface_uuid(&self, surface_ref: &str) -> Option<String> {
        self.surface_uuids.lock().unwrap().get(surface_ref).cloned()
    }

    /// Send a v2 JSON command over the blocking query socket.
    /// Returns the parsed response or an error.
    fn socket_send(&self, method: &str, params: &serde_json::Value) -> Result<serde_json::Value> {
        let mut guard = self.query_socket.lock().unwrap();

        // Connect if not connected
        if guard.is_none() {
            let path = Self::socket_path();
            debug!(path = %path.display(), "nsmux: connecting query socket");
            let stream = UnixStream::connect(&path)
                .with_context(|| format!("failed to connect to nsmux socket at {:?}", path))?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let reader = BufReader::new(stream.try_clone()?);
            *guard = Some(SocketConn {
                writer: stream,
                reader,
            });
        }

        let conn = guard.as_mut().unwrap();
        let request = serde_json::json!({
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        // Write request
        if let Err(e) = conn.writer.write_all(line.as_bytes()) {
            // Connection broken, drop it so we reconnect next time
            *guard = None;
            return Err(e.into());
        }

        // Read response
        let mut response_line = String::new();
        if let Err(e) = conn.reader.read_line(&mut response_line) {
            *guard = None;
            return Err(e.into());
        }

        let resp: serde_json::Value = serde_json::from_str(&response_line)?;
        if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            Ok(resp)
        } else {
            let msg = resp
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            Err(anyhow!("nsmux socket: {}", msg))
        }
    }

    /// Fire-and-forget a v2 JSON command over the socket.
    /// Writes the command but doesn't block waiting for the response.
    /// Stale responses are drained on the next call.
    fn socket_fire(&self, method: &str, params: &serde_json::Value) {
        let mut guard = self.input_socket.lock().unwrap();

        // Connect if not connected
        if guard.is_none() {
            let path = Self::socket_path();
            match UnixStream::connect(&path) {
                Ok(stream) => {
                    // Non-blocking reads so we never stall draining responses
                    let _ = stream.set_nonblocking(true);
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let reader = BufReader::new(stream.try_clone().unwrap());
                    *guard = Some(SocketConn { writer: stream, reader });
                }
                Err(_) => return,
            }
        }

        let conn = guard.as_mut().unwrap();

        // Drain any pending responses from previous fire-and-forget calls
        let mut drain_buf = String::new();
        loop {
            match conn.reader.read_line(&mut drain_buf) {
                Ok(0) => { *guard = None; return; } // EOF, connection closed
                Ok(_) => { drain_buf.clear(); continue; }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => { *guard = None; return; }
            }
        }

        let request = serde_json::json!({
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&request).unwrap();
        line.push('\n');

        if conn.writer.write_all(line.as_bytes()).is_err() {
            *guard = None;
        }
    }

    /// Resolve a surface UUID (from $CMUX_SURFACE_ID) to a `surface:N` ref.
    ///
    /// nsmux exposes UUIDs in env vars but uses `surface:N` short refs in
    /// tree output and most CLI commands. workmux needs a single canonical
    /// identifier, so we normalise to `surface:N` everywhere.
    fn resolve_surface_ref(&self, uuid: &str) -> Option<String> {
        // If it already looks like a short ref, pass through
        if uuid.starts_with("surface:") {
            return Some(uuid.to_string());
        }
        let output = self.cmux_query(&["identify", "--surface", uuid]).ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&output).ok()?;
        parsed
            .get("caller")
            .and_then(|c| c.get("surface_ref"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Run a cmux CLI command, returning an error with context on failure.
    fn cmux_cmd(&self, args: &[&str]) -> Result<()> {
        Cmd::new("cmux")
            .args(args)
            .run()
            .with_context(|| format!("cmux command failed: {:?}", args))?;
        Ok(())
    }

    /// Fire-and-forget a cmux CLI command (non-blocking).
    ///
    /// Used for send_key / send_keys so the dashboard event loop isn't
    /// blocked waiting for each subprocess to finish.
    fn cmux_fire(&self, args: &[&str]) {
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("cmux");
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.spawn(); // intentionally ignore result
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

    /// Get cached tree output, refreshing if stale.
    fn cached_tree(&self) -> Option<String> {
        {
            let cache = self.tree_cache.lock().ok()?;
            if let Some(ref cached) = *cache {
                if cached.fetched_at.elapsed() < TREE_CACHE_TTL {
                    return Some(cached.output.clone());
                }
            }
        }
        // Cache miss or stale -- fetch fresh
        let output = self.cmux_query(&["tree", "--all"]).ok()?;
        if let Ok(mut cache) = self.tree_cache.lock() {
            *cache = Some(TreeCache {
                output: output.clone(),
                fetched_at: Instant::now(),
            });
        }
        Some(output)
    }

    /// Resolve which workspace a surface belongs to by parsing `cmux tree --all`.
    /// Returns None if the surface isn't found (caller should proceed without --workspace).
    fn workspace_for_surface(&self, surface_ref: &str) -> Option<String> {
        let output = self.cached_tree()?;
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
        let output = match self.cached_tree() {
            Some(o) => o,
            None => return (None, None),
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
        let uuid = std::env::var("CMUX_SURFACE_ID").ok()?;
        // Resolve UUID to surface:N ref so it matches get_all_live_pane_info() keys
        self.resolve_surface_ref(&uuid).or(Some(uuid))
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

    fn respawn_pane(&self, pane_id: &str, _cwd: &Path, _cmd: Option<&str>) -> Result<String> {
        // nsmux's `respawn-pane` is just `surface.send_text` -- it types text into
        // the existing shell rather than killing and restarting the process like tmux.
        // The handshake script would arrive before the shell prompt is ready, causing
        // the FIFO pipe to never be written and the handshake to time out.
        //
        // Instead, we skip the respawn entirely. The shell spawned by `new-workspace`
        // is already a login shell. We just wait for it to finish loading (prompt ready)
        // by polling read-screen, then return. The caller will send_keys afterwards.
        let workspace_ref = self.workspace_for_surface(pane_id);

        for _ in 0..50 {
            let mut screen_args = vec!["read-screen", "--surface", pane_id, "--lines", "5"];
            if let Some(ws) = &workspace_ref {
                screen_args.insert(1, "--workspace");
                screen_args.insert(2, ws);
            }
            if let Ok(screen) = self.cmux_query(&screen_args) {
                let trimmed = screen.trim();
                if !trimmed.is_empty() {
                    return Ok(pane_id.to_string());
                }
            }
            thread::sleep(Duration::from_millis(100));
        }

        Ok(pane_id.to_string())
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        // Use surface UUID for cross-workspace correctness.
        // surface:N refs are workspace-relative in the v2 API.
        let mut params = serde_json::json!({
            "lines": lines,
            "scrollback": true,
        });
        if let Some(uuid) = self.surface_uuid(pane_id) {
            params["surface_id"] = serde_json::Value::String(uuid);
        } else {
            params["surface"] = serde_json::Value::String(pane_id.to_string());
        }
        let resp = self.socket_send("surface.read_text", &params).ok()?;
        resp.get("result")
            .and_then(|r| r.get("text"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    }

    // === Text I/O ===

    fn send_keys(&self, pane_id: &str, command: &str) -> Result<()> {
        // cmux send appends \n by default when text ends with it
        let text = format!("{}\\n", command);
        self.cmux_surface_cmd("send", pane_id, &[&text])
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        // Use socket for zero-fork overhead.
        // Resolve to UUID for cross-workspace correctness.
        let surface_param = if let Some(uuid) = self.surface_uuid(pane_id) {
            serde_json::json!({"surface_id": uuid})
        } else {
            serde_json::json!({"surface": pane_id})
        };

        let is_named_key = matches!(
            key,
            "Enter" | "backspace" | "Tab" | "Up" | "Down" | "Left" | "Right"
                | "Escape" | "Space" | "Home" | "End" | "pageup" | "pagedown"
                | "Delete" | "escape" | "enter" | "space" | "tab"
        ) || key.starts_with("ctrl+")
            || key.starts_with("alt+")
            || key.starts_with("shift+");

        if is_named_key {
            let mut params = surface_param;
            params["key"] = serde_json::Value::String(key.to_string());
            self.socket_fire("surface.send_key", &params);
        } else {
            let mut params = surface_param;
            params["text"] = serde_json::Value::String(key.to_string());
            self.socket_fire("surface.send_text", &params);
        }
        Ok(())
    }

    fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        let mut params = if let Some(uuid) = self.surface_uuid(pane_id) {
            serde_json::json!({"surface_id": uuid})
        } else {
            serde_json::json!({"surface": pane_id})
        };
        params["text"] = serde_json::Value::String(text.to_string());
        self.socket_fire("surface.send_text", &params);
        Ok(())
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

    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
        self.cmux_surface_cmd("send", pane_id, &[content])
    }

    // === Shell ===

    fn get_default_shell(&self) -> Result<String> {
        std::env::var("SHELL").or_else(|_| Ok("/bin/zsh".to_string()))
    }

    fn create_handshake(&self) -> Result<Box<dyn PaneHandshake>> {
        // nsmux's respawn_pane polls read-screen for shell readiness instead of
        // using a FIFO handshake (see respawn_pane comment). Return a no-op
        // handshake so the caller's wait() returns immediately.
        Ok(Box::new(NoopHandshake))
    }

    // === Status ===

    fn set_status(&self, pane_id: &str, icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        // Map workmux emoji icons to nsmux native status API (SF Symbols + color)
        let (value, nsmux_icon, color) = match icon {
            "🤖" => ("Working", "bolt.fill", "#4C8DFF"),
            "💬" => ("Waiting", "bell.fill", "#FFB84C"),
            "✅" => ("Done", "checkmark.circle.fill", "#4CAF50"),
            _ => ("Active", "circle.fill", "#888888"),
        };
        // Resolve pane to its workspace for scoped status
        let ws = self.workspace_for_surface(pane_id);
        let key = format!("workmux_{}", pane_id);
        let mut args = vec![
            "set-status", &key, value,
            "--icon", nsmux_icon,
            "--color", color,
        ];
        let ws_val;
        if let Some(ref w) = ws {
            ws_val = w.clone();
            args.push("--workspace");
            args.push(&ws_val);
        }
        let _ = self.cmux_cmd(&args);
        Ok(())
    }

    fn clear_status(&self, pane_id: &str) -> Result<()> {
        let ws = self.workspace_for_surface(pane_id);
        let key = format!("workmux_{}", pane_id);
        let mut args = vec!["clear-status", &key];
        let ws_val;
        if let Some(ref w) = ws {
            ws_val = w.clone();
            args.push("--workspace");
            args.push(&ws_val);
        }
        let _ = self.cmux_cmd(&args);
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

    fn server_boot_id(&self) -> Result<Option<String>> {
        // Use the socket file's inode as a boot ID. The inode changes
        // every time nsmux restarts (new socket file). This lets us
        // detect stale state files from previous nsmux sessions.
        let path = Self::socket_path();
        let metadata = std::fs::metadata(&path)?;
        use std::os::unix::fs::MetadataExt;
        Ok(Some(metadata.ino().to_string()))
    }

    fn discover_agents(&self) -> Result<()> {
        // Scan all surfaces for agent signatures (π in title = pi session).
        // Create state files for any that don't have one yet, so idle
        // agents appear in the dashboard immediately.
        use crate::state::{StateStore, PaneKey, AgentState};

        let live_panes = self.get_all_live_pane_info()?;
        let store = StateStore::new()?;
        let boot_id = self.server_boot_id().unwrap_or(None);
        let instance = self.instance_id();
        let backend = self.name().to_string();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        debug!(surfaces = live_panes.len(), "nsmux: discover_agents scanning surfaces");
        for (surface_ref, info) in &live_panes {
            // Check if title indicates a pi session (π prefix)
            let is_agent = info.title.as_ref()
                .map(|t| t.starts_with("π") || t.starts_with("\u{03C0}"))
                .unwrap_or(false);

            if !is_agent { continue; }

            let pane_key = PaneKey {
                backend: backend.clone(),
                instance: instance.clone(),
                pane_id: surface_ref.clone(),
            };

            // Skip if state already exists
            if let Ok(Some(_)) = store.get_agent(&pane_key) {
                continue;
            }

            info!(surface = %surface_ref, title = ?info.title, "nsmux: discovered agent session without state");
            // Create state for this discovered agent
            let state = AgentState {
                pane_key,
                workdir: info.working_dir.clone(),
                status: Some(super::AgentStatus::Done), // conservative default
                status_ts: Some(now),
                pane_title: info.title.clone(),
                pane_pid: 0,
                command: String::new(),
                updated_ts: now,
                window_name: info.window.clone(),
                session_name: info.session.clone(),
                boot_id: boot_id.clone(),
            };

            let _ = store.upsert_agent(&state);
        }

        Ok(())
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        // Use cmux identify to check if the surface exists.
        // For unknown UUIDs, identify returns caller.surface_ref: null.
        let output = match self.cmux_query(&["identify", "--surface", pane_id]) {
            Ok(text) => text,
            Err(_) => return Ok(None),
        };

        // Parse JSON and check if the surface was actually resolved
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output) {
            let caller_ref = parsed
                .get("caller")
                .and_then(|c| c.get("surface_ref"))
                .and_then(|v| v.as_str());
            if caller_ref.is_none() {
                // Surface UUID not found -- pane no longer exists
                return Ok(None);
            }

            let title = parsed
                .get("caller")
                .and_then(|c| c.get("workspace_ref"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            Ok(Some(LivePaneInfo {
                pid: None,
                current_command: None,
                working_dir: PathBuf::from(
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                ),
                title,
                session: None,
                window: None,
            }))
        } else {
            // Couldn't parse -- assume the surface doesn't exist
            Ok(None)
        }
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        // Use v2 system.tree for structured data including surface titles.
        let resp = self.socket_send("system.tree", &serde_json::json!({}))?;
        let mut result = HashMap::new();

        let windows = resp
            .get("result")
            .and_then(|r| r.get("windows"))
            .and_then(|w| w.as_array());

        if let Some(windows) = windows {
            for window in windows {
                let workspaces = window.get("workspaces").and_then(|w| w.as_array());
                if let Some(workspaces) = workspaces {
                    for ws in workspaces {
                        let ws_title = ws.get("title").and_then(|t| t.as_str()).map(|s| s.to_string());
                        for pane in ws.get("panes").and_then(|p| p.as_array()).unwrap_or(&vec![]) {
                            for surface in pane.get("surfaces").and_then(|s| s.as_array()).unwrap_or(&vec![]) {
                                let stype = surface.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                if stype != "terminal" { continue; }

                                let surface_ref = match surface.get("ref").and_then(|r| r.as_str()) {
                                    Some(r) if r.starts_with("surface:") => r.to_string(),
                                    _ => continue,
                                };
                                let title = surface.get("title").and_then(|t| t.as_str()).map(|s| s.to_string());
                                let uuid = surface.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());

                                // Cache the UUID for cross-workspace v2 API calls
                                if let Some(ref uuid) = uuid {
                                    self.surface_uuids.lock().unwrap()
                                        .insert(surface_ref.clone(), uuid.clone());
                                }

                                result.insert(surface_ref, LivePaneInfo {
                                    pid: None,
                                    current_command: None, // nsmux doesn't expose process info
                                    working_dir: PathBuf::from("/"),
                                    title,
                                    session: None,
                                    window: ws_title.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }

        debug!(surfaces = result.len(), "nsmux: get_all_live_pane_info");
        for (ref_str, info) in &result {
            debug!(surface = %ref_str, title = ?info.title, "nsmux: live surface");
        }
        Ok(result)
    }
}
