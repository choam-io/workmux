# Design: Group Dev Environments

> `workmux group` gains the ability to attach remote compute (starting with GitHub Codespaces) to a group workspace. The agent edits locally, commands execute remotely, and tunnels keep ports accessible.

## Motivation

The agent-stays-local / compute-runs-remote pattern needs first-class support. Today this is duct-taped together with `CS_TARGET`, csfleet, and manual SSH. The group workspace is the natural binding point: it already tracks repos, branches, and workspace directories. Adding `dev_env` makes remote compute a declared part of the workspace.

## Concepts

| Term | Meaning |
|------|---------|
| **dev_env** | A remote execution environment attached to a group. Currently: codespace. |
| **tunnel** | A per-port SSH tunnel from a local port to a remote port. Managed by a background watcher process. |
| **port pool** | Global sequential allocator for local ports. Prevents collisions across concurrent groups. |
| **watcher** | Background process that babysits SSH tunnel child processes for a group. Spawned by CLI, killed by CLI. |

## Config

In `~/.config/workmux/config.yaml`:

```yaml
groups:
  choam:
    repos:
      - path: ~/ghq/github.com/choam-io/cmux
      - path: ~/ghq/github.com/choam-io/workmux
      - path: ~/ghq/github.com/choam-io/deck
    merge_order: [cmux, workmux, deck]
    dev_env:
      type: codespace
      repo: acme-corp/webapp
      machine: standardLinux32gb
      location: WestUs2                    # optional
      devcontainer_path: .devcontainer/devcontainer.json  # optional
      idle_timeout: 30m                    # optional
      retention_period: 72h               # optional
      ports: [3000, 3001, 6379]           # remote ports to forward
      sync: git-push
      devloop: |
        Edit locally, commit, push. Pull on remote before build/test.
        Build: ssh <ssh_host> "cd <remote_workdir> && git pull && make build"
        Test:  ssh <ssh_host> "cd <remote_workdir> && git pull && go test ./..."

  launch:
    repos:
      - path: ~/ghq/github.com/acme-corp/monolith
    dev_env:
      type: codespace
      repo: acme-corp/monolith
      machine: xLargePremiumLinux256gb
      devcontainer_path: .devcontainer/actions/devcontainer.json
      ports: [3000, 3001, 1234, 9200]
      sync: mutagen
      devloop: |
        Files sync automatically via mutagen. No git push needed.
        Build: ssh <ssh_host> "cd <remote_workdir> && script/server -q"
        Test:  ssh <ssh_host> "cd <remote_workdir> && bin/rails test <file>"
```

### Codespace creation parameters

All fields under `dev_env` for `type: codespace`:

| Field | Type | Required | Maps to |
|-------|------|----------|---------|
| `type` | string | yes | — |
| `repo` | string | yes | `gh cs create -R` |
| `machine` | string | no | `gh cs create -m` |
| `branch` | string | no | `gh cs create -b` (defaults to repo default branch) |
| `location` | string | no | `gh cs create -l` (EastUs, SouthEastAsia, WestEurope, WestUs2) |
| `devcontainer_path` | string | no | `gh cs create --devcontainer-path` |
| `display_name` | string | no | `gh cs create -d` (48 chars max) |
| `idle_timeout` | string | no | `gh cs create --idle-timeout` (e.g. "30m", "1h") |
| `retention_period` | string | no | `gh cs create --retention-period` (max 30 days) |
| `ports` | list[int] | no | Remote ports to forward locally |
| `sync` | string | no | How changes reach the codespace: `git-push` (default), `mutagen`, `rsync`, `none` |
| `devloop` | string | no | Inline prompt telling the agent how the dev cycle works. Injected into agent context. |

## Core Principle: Local is Source of Truth

The agent edits files locally. The codespace is a deployment target for build, test, and run. Changes flow one-way: local → remote. Never edit on the codespace. Never pull changes from the codespace back to local.

The `sync` field declares how changes reach the codespace:

| sync | Mechanism | When changes appear remotely |
|------|-----------|------------------------------|
| `git-push` | Agent commits + pushes, pulls on remote | On explicit push (default) |
| `mutagen` | Watcher manages mutagen session | Immediately (filesystem watch) |
| `rsync` | Agent runs rsync before build/test | On explicit sync |
| `none` | Manual. devloop must explain. | Never automatic |

The `devloop` field is an inline prompt injected into the agent's context. It tells the agent exactly how to work in this group. Different groups can have completely different devloops.

## Runtime State

### Group state (`.workmux-group.yaml`)

```yaml
group_name: choam
branch: feat/something
repos:
  - repo_path: /Users/me/ghq/github.com/choam-io/cmux
    worktree_path: /Users/me/ghq/github.com/choam-io/cmux__worktrees/feat-something
    branch: feat/something
    symlink_name: cmux
  # ...
dev_env:
  type: codespace
  repo: acme-corp/webapp
  machine: standardLinux32gb
  sync: git-push
  devloop: |
    Edit locally, commit, push. Pull on remote before build/test.
    Build: ssh <ssh_host> "cd <remote_workdir> && git pull && make build"
    Test:  ssh <ssh_host> "cd <remote_workdir> && git pull && go test ./..."
  # runtime fields (written after attach)
  codespace_name: webapp-abc123
  ssh_host: cs.webapp-abc123
  remote_workdir: /workspaces/webapp
  attached_at: 1775099653
  ports:
    - remote: 3000
      local: 10000
    - remote: 3001
      local: 10001
    - remote: 6379
      local: 10002
  watcher_pid: 48291
```

### Global port pool (`~/.local/share/workmux/ports.yaml`)

```yaml
pool_start: 10000
pool_end: 19999
allocations:
  - group: choam
    branch: feat/something
    ports: [10000, 10001, 10002]
  - group: launch
    branch: feat/y
    ports: [10003, 10004, 10005, 10006]
```

Sequential allocation. On `group remove`, ports are released. On daemon startup, stale allocations (workspace dir gone) are garbage collected.

## Lifecycle

### Happy path

```
workmux group add choam feat/x
  1. Create worktrees across repos (existing behavior)
  2. Config has dev_env → begin attach:
     a. Find or create codespace (gh cs create / gh cs list)
     b. Ensure codespace is running (gh cs start if stopped)
     c. Set up SSH config (gh cs ssh --config)
     d. Allocate local ports from pool
     e. Spawn watcher (background process, per-port SSH tunnels)
     f. Write dev_env state to .workmux-group.yaml
  3. Launch agent in workspace

Agent session:
  - Reads .workmux-group.yaml → knows codespace name, SSH host, port map
  - SSH into codespace, starts services
  - Tunnels carry traffic as services come up
  - Agent works...

workmux group remove choam feat/x
  1. Kill watcher (kills all tunnel child processes)
  2. Release ports back to pool
  3. Stop codespace (gh cs stop)
  4. Remove worktrees (existing behavior)
```

### Manual override

```bash
# Skip dev_env even though config defines one
workmux group add choam feat/local-only --no-dev-env

# Override codespace settings at add time
workmux group add choam feat/investigate \
  --dev-env-repo acme-corp/monolith \
  --dev-env-machine xLargePremiumLinux256gb \
  --dev-env-ports 3000,3001,1234

# Attach to existing codespace by name
workmux group dev-env attach --codespace existing-cs-name --ports 3000,8080

# Detach without removing group
workmux group dev-env detach

# Status / port map
workmux group dev-env status
workmux group dev-env ports
```

## Watcher

The watcher is a background child process, not a system daemon. Spawned by `group add` or `dev-env attach`, killed by `group remove` or `dev-env detach`.

### Per-port SSH tunnels

Each port gets its own SSH process:

```
ssh -N -L 10000:localhost:3000 cs.webapp-abc123
ssh -N -L 10001:localhost:3001 cs.webapp-abc123
ssh -N -L 10002:localhost:6379 cs.webapp-abc123
```

Tunnels bind the local port immediately. Connections reset until the remote service is listening. No coordination needed.

### Reconnect

```
SSH child exits
  → wait 2s, respawn
  → if respawn fails immediately (codespace stopped/unreachable):
      backoff: 2s → 4s → 8s → 16s → cap at 30s
  → if respawn succeeds:
      reset backoff to 2s
```

### Observability

The watcher writes structured logs (JSON lines) to:
```
~/.local/share/workmux/watcher/<group>--<branch>/watcher.log
```

Each tunnel logs state transitions:
```jsonl
{"ts":"...","tunnel":"3000→10000","event":"started","pid":48301}
{"ts":"...","tunnel":"3000→10000","event":"died","exit_code":255}
{"ts":"...","tunnel":"3000→10000","event":"reconnecting","attempt":1,"backoff_ms":2000}
{"ts":"...","tunnel":"3000→10000","event":"started","pid":48305}
{"ts":"...","tunnel":"6379→10002","event":"started","pid":48302}
```

`workmux group dev-env status` reads these logs + checks process liveness:

```
Dev Environment: codespace (acme-corp/webapp)
  Codespace:  webapp-abc123
  SSH host:   cs.webapp-abc123
  Watcher:    PID 48291 (running)

  Tunnels:
    3000 → localhost:10000  ✓ connected (PID 48301)
    3001 → localhost:10001  ✓ connected (PID 48303)
    6379 → localhost:10002  ✗ reconnecting (attempt 3, backoff 8s)
```

### Crash recovery

None needed. If watcher dies, tunnels die. Agent notices SSH/HTTP failures, re-runs `workmux group dev-env attach`. The agent is the lifecycle owner.

## Rust Module Layout

```
src/
  dev_env/
    mod.rs              # DevEnvConfig, DevEnvState, DevEnvPortMapping types
    codespace.rs        # codespace lifecycle (create/reuse/start/stop/SSH config)
    ports.rs            # global port pool (allocate/release/persist/gc)
    tunnel.rs           # per-port SSH tunnel spawn
    watcher.rs          # background process: event loop, tunnel set, reconnect, logging
  command/
    dev_env.rs          # CLI: attach, detach, status, ports
  workflow/
    group.rs            # existing — modified to integrate dev_env on add/remove
```

### Key types

```rust
/// Config-level dev_env definition (from config.yaml)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DevEnvConfig {
    Codespace {
        repo: String,
        #[serde(default)]
        machine: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        location: Option<String>,
        #[serde(default)]
        devcontainer_path: Option<String>,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        idle_timeout: Option<String>,
        #[serde(default)]
        retention_period: Option<String>,
        #[serde(default)]
        ports: Vec<u16>,
        #[serde(default = "default_sync")]
        sync: SyncStrategy,
        #[serde(default)]
        devloop: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SyncStrategy {
    #[default]
    GitPush,
    Mutagen,
    Rsync,
    None,
}

/// Runtime state written to .workmux-group.yaml after attach
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevEnvState {
    #[serde(flatten)]
    pub config: DevEnvConfig,
    /// Runtime fields (None before attach)
    #[serde(default)]
    pub codespace_name: Option<String>,
    #[serde(default)]
    pub ssh_host: Option<String>,
    #[serde(default)]
    pub remote_workdir: Option<String>,
    #[serde(default)]
    pub attached_at: Option<u64>,
    #[serde(default)]
    pub port_mappings: Vec<PortMapping>,
    #[serde(default)]
    pub watcher_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortMapping {
    pub remote: u16,
    pub local: u16,
}

/// Global port pool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortPool {
    pub pool_start: u16,
    pub pool_end: u16,
    pub allocations: Vec<PortAllocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortAllocation {
    pub group: String,
    pub branch: String,
    pub ports: Vec<u16>,
}
```

## CLI Surface

```
workmux group add <group> <branch>
  [--no-dev-env]                         # skip dev_env even if config has one
  [--dev-env-repo <owner/repo>]          # override codespace repo
  [--dev-env-machine <machine>]          # override machine type
  [--dev-env-ports <port,port,...>]       # override forwarded ports
  [--dev-env-devcontainer <path>]        # override devcontainer path

workmux group dev-env attach
  [--codespace <name>]                   # attach to existing codespace
  [--repo <owner/repo>]                  # create for this repo
  [--machine <machine>]                  # machine type
  [--ports <port,port,...>]              # ports to forward

workmux group dev-env detach             # stop codespace, kill tunnels, release ports

workmux group dev-env status             # codespace state + tunnel health + port map

workmux group dev-env ports              # just the port map (quick reference)
  --json                                 # machine-readable output

workmux group remove <group> <branch>    # existing — now also tears down dev_env
```

## Future extensions

- `type: ssh` — attach to any SSH host, not just codespace
- `type: lima` — local VM via Lima
- File sync (mutagen) managed by the watcher
- `workmux group dev-env exec <command>` — run a one-off command on the remote
