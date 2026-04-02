//! Port pool garbage collection utilities.
//! Global port pool for allocating local ports across concurrent groups.
//!
//! Sequential allocation from a configurable range (default 10000-19999).
//! State persisted to ~/.local/share/workmux/ports.yaml.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use tracing::{debug, warn};

use crate::workflow::group;

const DEFAULT_POOL_START: u16 = 10000;
const DEFAULT_POOL_END: u16 = 19999;

/// A port allocation for one group workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortAllocation {
    pub group: String,
    pub branch: String,
    pub ports: Vec<u16>,
}

/// Global port pool state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortPool {
    #[serde(default = "default_pool_start")]
    pub pool_start: u16,
    #[serde(default = "default_pool_end")]
    pub pool_end: u16,
    #[serde(default)]
    pub allocations: Vec<PortAllocation>,
}

fn default_pool_start() -> u16 {
    DEFAULT_POOL_START
}

fn default_pool_end() -> u16 {
    DEFAULT_POOL_END
}

impl Default for PortPool {
    fn default() -> Self {
        Self {
            pool_start: DEFAULT_POOL_START,
            pool_end: DEFAULT_POOL_END,
            allocations: Vec::new(),
        }
    }
}

impl PortPool {
    /// Path to the port pool state file.
    fn state_path() -> Result<PathBuf> {
        let home = home::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
        Ok(home.join(".local/share/workmux/ports.yaml"))
    }

    /// Load the port pool from disk. Returns default if file doesn't exist.
    pub fn load() -> Result<Self> {
        let path = Self::state_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read port pool from {}", path.display()))?;
        let mut pool: Self = serde_yaml::from_str(&content)
            .context("Failed to parse port pool YAML")?;
        pool.gc();
        Ok(pool)
    }

    /// Save the port pool to disk (atomic write).
    pub fn save(&self) -> Result<()> {
        let path = Self::state_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_yaml::to_string(self)
            .context("Failed to serialize port pool")?;
        let tmp = path.with_extension("yaml.tmp");
        fs::write(&tmp, &content)
            .context("Failed to write temp port pool file")?;
        fs::rename(&tmp, &path)
            .context("Failed to rename temp port pool file")?;
        Ok(())
    }

    /// Collect all currently allocated ports.
    fn allocated_ports(&self) -> HashSet<u16> {
        self.allocations
            .iter()
            .flat_map(|a| a.ports.iter().copied())
            .collect()
    }

    /// Allocate `count` sequential ports for a group workspace.
    pub fn allocate(
        &mut self,
        group_name: &str,
        branch: &str,
        count: usize,
    ) -> Result<Vec<u16>> {
        if count == 0 {
            return Ok(Vec::new());
        }

        let used = self.allocated_ports();
        let mut allocated = Vec::with_capacity(count);

        for port in self.pool_start..=self.pool_end {
            if !used.contains(&port) {
                allocated.push(port);
                if allocated.len() == count {
                    break;
                }
            }
        }

        if allocated.len() < count {
            bail!(
                "Port pool exhausted: need {} ports, only {} available in range {}-{}",
                count,
                allocated.len(),
                self.pool_start,
                self.pool_end,
            );
        }

        debug!(
            group = group_name,
            branch = branch,
            ports = ?allocated,
            "port_pool:allocated"
        );

        self.allocations.push(PortAllocation {
            group: group_name.to_string(),
            branch: branch.to_string(),
            ports: allocated.clone(),
        });

        Ok(allocated)
    }

    /// Release ports for a group workspace.
    pub fn release(&mut self, group_name: &str, branch: &str) {
        let before = self.allocations.len();
        self.allocations
            .retain(|a| !(a.group == group_name && a.branch == branch));
        let released = before - self.allocations.len();
        if released > 0 {
            debug!(
                group = group_name,
                branch = branch,
                released,
                "port_pool:released"
            );
        }
    }

    /// Garbage collect stale allocations whose group workspace dir no longer exists.
    fn gc(&mut self) {
        let before = self.allocations.len();
        self.allocations.retain(|alloc| {
            match group::workspace_dir(&alloc.group, &alloc.branch) {
                Ok(dir) => {
                    if dir.exists() {
                        true
                    } else {
                        warn!(
                            group = alloc.group,
                            branch = alloc.branch,
                            "port_pool:gc:stale allocation, workspace dir gone"
                        );
                        false
                    }
                }
                Err(_) => false,
            }
        });
        let removed = before - self.allocations.len();
        if removed > 0 {
            debug!(removed, "port_pool:gc:cleaned stale allocations");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> PortPool {
        PortPool {
            pool_start: 10000,
            pool_end: 10099,
            allocations: Vec::new(),
        }
    }

    #[test]
    fn allocate_sequential() {
        let mut pool = test_pool();
        let ports = pool.allocate("group1", "feat/a", 3).unwrap();
        assert_eq!(ports, vec![10000, 10001, 10002]);
    }

    #[test]
    fn allocate_avoids_used() {
        let mut pool = test_pool();
        pool.allocate("group1", "feat/a", 3).unwrap();
        let ports = pool.allocate("group2", "feat/b", 2).unwrap();
        assert_eq!(ports, vec![10003, 10004]);
    }

    #[test]
    fn allocate_exhausted() {
        let mut pool = PortPool {
            pool_start: 10000,
            pool_end: 10001,
            allocations: Vec::new(),
        };
        pool.allocate("g", "b", 2).unwrap();
        let result = pool.allocate("g2", "b2", 1);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exhausted"));
    }

    #[test]
    fn allocate_zero() {
        let mut pool = test_pool();
        let ports = pool.allocate("g", "b", 0).unwrap();
        assert!(ports.is_empty());
        assert!(pool.allocations.is_empty());
    }

    #[test]
    fn release_frees_ports() {
        let mut pool = test_pool();
        pool.allocate("group1", "feat/a", 3).unwrap();
        pool.allocate("group2", "feat/b", 2).unwrap();

        pool.release("group1", "feat/a");
        assert_eq!(pool.allocations.len(), 1);
        assert_eq!(pool.allocations[0].group, "group2");

        // Freed ports can be reused
        let ports = pool.allocate("group3", "feat/c", 3).unwrap();
        assert_eq!(ports, vec![10000, 10001, 10002]);
    }

    #[test]
    fn release_nonexistent_is_noop() {
        let mut pool = test_pool();
        pool.allocate("g", "b", 1).unwrap();
        pool.release("nonexistent", "branch");
        assert_eq!(pool.allocations.len(), 1);
    }

    #[test]
    fn serde_roundtrip() {
        let mut pool = test_pool();
        pool.allocate("choam", "feat/x", 3).unwrap();
        pool.allocate("launch", "feat/y", 4).unwrap();

        let yaml = serde_yaml::to_string(&pool).unwrap();
        let loaded: PortPool = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(loaded.pool_start, 10000);
        assert_eq!(loaded.pool_end, 10099);
        assert_eq!(loaded.allocations.len(), 2);
        assert_eq!(loaded.allocations[0].ports, vec![10000, 10001, 10002]);
        assert_eq!(
            loaded.allocations[1].ports,
            vec![10003, 10004, 10005, 10006]
        );
    }

    #[test]
    fn default_pool() {
        let pool = PortPool::default();
        assert_eq!(pool.pool_start, 10000);
        assert_eq!(pool.pool_end, 19999);
        assert!(pool.allocations.is_empty());
    }
}
