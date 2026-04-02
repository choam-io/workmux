//! Per-port SSH tunnel management.
//!
//! Each remote port gets its own `ssh -N -L` process.

use anyhow::{Context, Result};
use std::process::{Child, Command, Stdio};
use tracing::debug;

/// A running SSH tunnel for one port.
pub struct Tunnel {
    pub remote_port: u16,
    pub local_port: u16,
    pub ssh_host: String,
    pub child: Child,
}

impl Tunnel {
    /// Spawn an SSH tunnel: local_port → remote_port via ssh_host.
    pub fn spawn(ssh_host: &str, local_port: u16, remote_port: u16) -> Result<Self> {
        let forward_spec = format!("{}:localhost:{}", local_port, remote_port);

        debug!(
            ssh_host,
            local_port,
            remote_port,
            "tunnel:spawning"
        );

        let child = Command::new("ssh")
            .args([
                "-N",                         // no remote command
                "-o", "ExitOnForwardFailure=yes",
                "-o", "ServerAliveInterval=15",
                "-o", "ServerAliveCountMax=3",
                "-L", &forward_spec,
                ssh_host,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "Failed to spawn SSH tunnel {}→{} via {}",
                    remote_port, local_port, ssh_host
                )
            })?;

        debug!(
            pid = child.id(),
            local_port,
            remote_port,
            "tunnel:started"
        );

        Ok(Self {
            remote_port,
            local_port,
            ssh_host: ssh_host.to_string(),
            child,
        })
    }

    /// Check if the tunnel process is still alive.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Get the PID of the tunnel process.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Kill the tunnel process.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.kill();
    }
}
