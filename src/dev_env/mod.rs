//! Dev environment support for group workspaces.
//!
//! Allows attaching remote compute (currently GitHub Codespaces) to a group,
//! with managed SSH tunnels, port forwarding, and sync strategies.

pub mod codespace;
pub mod ports;
pub mod tunnel;
pub mod watcher;

use serde::{Deserialize, Serialize};

/// How changes reach the remote environment.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SyncStrategy {
    /// Agent commits + pushes locally, pulls on remote before build/test.
    #[default]
    GitPush,
    /// Mutagen watches the local worktree and syncs in real-time.
    Mutagen,
    /// Agent runs rsync before remote operations.
    Rsync,
    /// No automatic sync. The devloop field must explain.
    None,
}

/// Config-level dev_env definition (from config.yaml group section).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DevEnvConfig {
    Codespace {
        /// Repository for the codespace (e.g., "acme-corp/webapp")
        repo: String,
        /// Machine type (e.g., "standardLinux32gb")
        #[serde(default)]
        machine: Option<String>,
        /// Automatically attach dev env on `group add`. Default: false.
        #[serde(default)]
        auto_attach: bool,
        /// Git branch for the codespace
        #[serde(default)]
        branch: Option<String>,
        /// Location: EastUs, SouthEastAsia, WestEurope, WestUs2
        #[serde(default)]
        location: Option<String>,
        /// Path to devcontainer.json within the repo
        #[serde(default)]
        devcontainer_path: Option<String>,
        /// Display name (48 chars max)
        #[serde(default)]
        display_name: Option<String>,
        /// Auto-stop after inactivity (e.g., "30m", "1h")
        #[serde(default)]
        idle_timeout: Option<String>,
        /// Auto-delete after shutdown (max 30 days, e.g., "72h")
        #[serde(default)]
        retention_period: Option<String>,
        /// Remote ports to forward locally
        #[serde(default)]
        ports: Vec<u16>,
        /// How changes reach the codespace
        #[serde(default)]
        sync: SyncStrategy,
        /// Inline prompt telling the agent how the dev cycle works
        #[serde(default)]
        devloop: Option<String>,
    },
}

impl DevEnvConfig {
    /// Whether auto-attach on group add is enabled.
    pub fn auto_attach(&self) -> bool {
        match self {
            DevEnvConfig::Codespace { auto_attach, .. } => *auto_attach,
        }
    }

    /// Get the list of remote ports to forward.
    pub fn ports(&self) -> &[u16] {
        match self {
            DevEnvConfig::Codespace { ports, .. } => ports,
        }
    }

    /// Get the sync strategy.
    pub fn sync_strategy(&self) -> &SyncStrategy {
        match self {
            DevEnvConfig::Codespace { sync, .. } => sync,
        }
    }

    /// Get the devloop prompt, if any.
    pub fn devloop(&self) -> Option<&str> {
        match self {
            DevEnvConfig::Codespace { devloop, .. } => devloop.as_deref(),
        }
    }
}

/// A single port mapping: remote port on codespace → local port on this machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortMapping {
    pub remote: u16,
    pub local: u16,
}

/// Runtime state for an attached dev environment.
/// Written to .workmux-group.yaml after attach.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevEnvState {
    /// The config that was used (includes type tag)
    #[serde(flatten)]
    pub config: DevEnvConfig,

    // -- Runtime fields (populated after attach) --
    /// Codespace name (assigned by GitHub)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codespace_name: Option<String>,
    /// SSH host alias (already in ~/.ssh/config)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host: Option<String>,
    /// Working directory on the remote
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_workdir: Option<String>,
    /// Unix timestamp when attached
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_at: Option<u64>,
    /// Port mappings (remote → allocated local)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub port_mappings: Vec<PortMapping>,
    /// PID of the tunnel watcher process
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watcher_pid: Option<u32>,
}

impl DevEnvState {
    /// Create a new state from config (runtime fields empty, pre-attach).
    pub fn from_config(config: DevEnvConfig) -> Self {
        Self {
            config,
            codespace_name: None,
            ssh_host: None,
            remote_workdir: None,
            attached_at: None,
            port_mappings: Vec::new(),
            watcher_pid: None,
        }
    }

    /// Whether the dev env has been attached (runtime fields populated).
    pub fn is_attached(&self) -> bool {
        self.codespace_name.is_some() && self.ssh_host.is_some()
    }

    /// Get the SSH host, if attached.
    pub fn ssh_host(&self) -> Option<&str> {
        self.ssh_host.as_deref()
    }

    /// Get the codespace name, if attached.
    pub fn codespace_name(&self) -> Option<&str> {
        self.codespace_name.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_codespace_roundtrip() {
        let yaml = r#"
type: codespace
repo: acme-corp/webapp
machine: standardLinux32gb
ports: [3000, 3001, 6379]
sync: git-push
devloop: |
  Edit locally, push, pull on remote.
"#;
        let config: DevEnvConfig = serde_yaml::from_str(yaml).unwrap();
        match &config {
            DevEnvConfig::Codespace {
                repo,
                machine,
                ports,
                sync,
                devloop,
                ..
            } => {
                assert_eq!(repo, "acme-corp/webapp");
                assert_eq!(machine.as_deref(), Some("standardLinux32gb"));
                assert_eq!(ports, &[3000, 3001, 6379]);
                assert_eq!(sync, &SyncStrategy::GitPush);
                assert!(devloop.as_ref().unwrap().contains("Edit locally"));
            }
        }

        // Roundtrip
        let serialized = serde_yaml::to_string(&config).unwrap();
        let _: DevEnvConfig = serde_yaml::from_str(&serialized).unwrap();
    }

    #[test]
    fn config_minimal_codespace() {
        let yaml = r#"
type: codespace
repo: org/repo
"#;
        let config: DevEnvConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.ports(), &[] as &[u16]);
        assert_eq!(config.sync_strategy(), &SyncStrategy::GitPush);
        assert!(config.devloop().is_none());
    }

    #[test]
    fn sync_strategy_serde() {
        for (input, expected) in [
            ("git-push", SyncStrategy::GitPush),
            ("mutagen", SyncStrategy::Mutagen),
            ("rsync", SyncStrategy::Rsync),
            ("none", SyncStrategy::None),
        ] {
            let parsed: SyncStrategy =
                serde_yaml::from_str(&format!("\"{}\"", input)).unwrap();
            assert_eq!(parsed, expected);

            let serialized = serde_yaml::to_string(&expected).unwrap();
            assert!(serialized.trim().contains(input));
        }
    }

    #[test]
    fn state_from_config() {
        let config = DevEnvConfig::Codespace {
            repo: "org/repo".to_string(),
            machine: None,
            auto_attach: false,
            branch: None,
            location: None,
            devcontainer_path: None,
            display_name: None,
            idle_timeout: None,
            retention_period: None,
            ports: vec![3000],
            sync: SyncStrategy::GitPush,
            devloop: None,
        };

        let state = DevEnvState::from_config(config);
        assert!(!state.is_attached());
        assert!(state.ssh_host().is_none());
        assert!(state.codespace_name().is_none());
        assert!(state.port_mappings.is_empty());
    }

    #[test]
    fn state_roundtrip_with_runtime_fields() {
        let state = DevEnvState {
            config: DevEnvConfig::Codespace {
                repo: "acme-corp/webapp".to_string(),
                machine: Some("standardLinux32gb".to_string()),
                auto_attach: false,
                branch: None,
                location: None,
                devcontainer_path: None,
                display_name: None,
                idle_timeout: None,
                retention_period: None,
                ports: vec![3000, 6379],
                sync: SyncStrategy::Mutagen,
                devloop: Some("just build".to_string()),
            },
            codespace_name: Some("webapp-abc123".to_string()),
            ssh_host: Some("cs.webapp-abc123".to_string()),
            remote_workdir: Some("/workspaces/webapp".to_string()),
            attached_at: Some(1775099653),
            port_mappings: vec![
                PortMapping {
                    remote: 3000,
                    local: 10000,
                },
                PortMapping {
                    remote: 6379,
                    local: 10001,
                },
            ],
            watcher_pid: Some(48291),
        };

        assert!(state.is_attached());
        assert_eq!(state.ssh_host(), Some("cs.webapp-abc123"));
        assert_eq!(state.codespace_name(), Some("webapp-abc123"));

        let yaml = serde_yaml::to_string(&state).unwrap();
        let loaded: DevEnvState = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(loaded.codespace_name, state.codespace_name);
        assert_eq!(loaded.port_mappings.len(), 2);
        assert_eq!(loaded.watcher_pid, Some(48291));
    }

    #[test]
    fn port_mapping_serde() {
        let mapping = PortMapping {
            remote: 3000,
            local: 10000,
        };
        let yaml = serde_yaml::to_string(&mapping).unwrap();
        let loaded: PortMapping = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(loaded, mapping);
    }
}
