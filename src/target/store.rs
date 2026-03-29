//! Filesystem-based persistence for execution target state.

use anyhow::{Context, Result};
use std::fs;
use std::io;
use std::path::PathBuf;
use tracing::{debug, warn};

use super::types::TargetState;
use crate::state::store::get_state_dir;

/// Manages filesystem-based persistence for execution targets.
///
/// Directory structure:
/// ```text
/// $XDG_STATE_HOME/workmux/
/// └── targets/
///     └── <handle>.json          # Target state per worktree
/// ```
pub struct TargetStore {
    targets_dir: PathBuf,
}

impl TargetStore {
    /// Create a new TargetStore.
    pub fn new() -> Result<Self> {
        let base = get_state_dir()?.join("workmux").join("targets");
        fs::create_dir_all(&base).context("Failed to create targets directory")?;
        Ok(Self { targets_dir: base })
    }

    /// Path to a target state file.
    fn target_path(&self, handle: &str) -> PathBuf {
        self.targets_dir.join(format!("{}.json", handle))
    }

    /// Save target state for a worktree.
    pub fn save(&self, state: &TargetState) -> Result<()> {
        let path = self.target_path(&state.handle);
        let content = serde_json::to_string_pretty(state)?;
        write_atomic(&path, content.as_bytes())
    }

    /// Load target state for a worktree.
    pub fn load(&self, handle: &str) -> Result<Option<TargetState>> {
        let path = self.target_path(handle);
        match fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(state) => Ok(Some(state)),
                Err(e) => {
                    warn!(?path, error = %e, "corrupted target state, deleting");
                    let _ = fs::remove_file(&path);
                    Ok(None)
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("Failed to read target state"),
        }
    }

    /// Delete target state for a worktree.
    pub fn delete(&self, handle: &str) -> Result<()> {
        let path = self.target_path(handle);
        match fs::remove_file(&path) {
            Ok(()) => {
                debug!(handle, "deleted target state");
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("Failed to delete target state"),
        }
    }

    /// List all stored targets.
    pub fn list_all(&self) -> Result<Vec<TargetState>> {
        if !self.targets_dir.exists() {
            return Ok(Vec::new());
        }

        let mut targets = Vec::new();
        for entry in fs::read_dir(&self.targets_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                match fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str(&content) {
                        Ok(state) => targets.push(state),
                        Err(e) => {
                            warn!(?path, error = %e, "corrupted target state, deleting");
                            let _ = fs::remove_file(&path);
                        }
                    },
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        warn!(?path, error = %e, "failed to read target state");
                    }
                    _ => {}
                }
            }
        }
        Ok(targets)
    }
}

/// Write content atomically using temp file + rename.
fn write_atomic(path: &std::path::Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, content).context("Failed to write temp file")?;
    fs::rename(&tmp, path).context("Failed to rename temp file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::TargetType;
    use tempfile::TempDir;

    fn test_store() -> (TargetStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let targets_dir = dir.path().join("targets");
        fs::create_dir_all(&targets_dir).unwrap();
        (TargetStore { targets_dir }, dir)
    }

    fn test_target_state() -> TargetState {
        TargetState {
            handle: "my-feature".to_string(),
            worktree_path: PathBuf::from("/home/user/project__worktrees/my-feature"),
            target: TargetType::Codespace {
                repo: "acme-corp/webapp".to_string(),
                codespace_name: "webapp-abc123".to_string(),
                ssh_host: "cs.webapp-abc123".to_string(),
            },
            attached_ts: 1234567890,
            remote_workdir: Some(PathBuf::from("/workspaces/webapp")),
        }
    }

    #[test]
    fn test_save_and_load() {
        let (store, _dir) = test_store();
        let state = test_target_state();

        store.save(&state).unwrap();

        let loaded = store.load("my-feature").unwrap().unwrap();
        assert_eq!(loaded.handle, "my-feature");
        assert_eq!(loaded.codespace_name(), Some("webapp-abc123"));
    }

    #[test]
    fn test_load_nonexistent() {
        let (store, _dir) = test_store();
        let result = store.load("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_delete() {
        let (store, _dir) = test_store();
        let state = test_target_state();

        store.save(&state).unwrap();
        assert!(store.load("my-feature").unwrap().is_some());

        store.delete("my-feature").unwrap();
        assert!(store.load("my-feature").unwrap().is_none());
    }

    #[test]
    fn test_list_all() {
        let (store, _dir) = test_store();

        let state1 = test_target_state();
        let mut state2 = test_target_state();
        state2.handle = "another-feature".to_string();

        store.save(&state1).unwrap();
        store.save(&state2).unwrap();

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 2);
    }
}
