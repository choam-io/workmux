//! Types for execution targets.
//!
//! An execution target is where commands run. The agent stays local (edits files,
//! orchestrates work), but build/test/deploy commands execute on the target.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Type of execution target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TargetType {
    /// Run commands locally (default, no target attached)
    Local,
    /// Run commands in a GitHub Codespace via SSH
    Codespace {
        /// Repository for the codespace (e.g., "acme-corp/webapp")
        repo: String,
        /// Codespace name (auto-generated or user-specified)
        codespace_name: String,
        /// SSH host entry for this codespace
        ssh_host: String,
    },
}

impl Default for TargetType {
    fn default() -> Self {
        Self::Local
    }
}

/// Persisted state for an execution target attached to a worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetState {
    /// The worktree handle (directory name)
    pub handle: String,

    /// Absolute path to the worktree on the local machine
    pub worktree_path: PathBuf,

    /// The target configuration
    pub target: TargetType,

    /// Unix timestamp when the target was attached
    pub attached_ts: u64,

    /// Remote working directory (for codespaces, the workspace path)
    pub remote_workdir: Option<PathBuf>,
}

impl TargetState {
    /// Get the SSH host for this target, if applicable.
    pub fn ssh_host(&self) -> Option<&str> {
        match &self.target {
            TargetType::Local => None,
            TargetType::Codespace { ssh_host, .. } => Some(ssh_host),
        }
    }

    /// Get the codespace name, if applicable.
    pub fn codespace_name(&self) -> Option<&str> {
        match &self.target {
            TargetType::Local => None,
            TargetType::Codespace { codespace_name, .. } => Some(codespace_name),
        }
    }

    /// Check if this is a local target.
    pub fn is_local(&self) -> bool {
        matches!(self.target, TargetType::Local)
    }
}
