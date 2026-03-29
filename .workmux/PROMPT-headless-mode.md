# Phase 0: Headless Mode Refactor

## Goal

Make workmux's worktree creation work **without a multiplexer** (no tmux, no nsmux). Today `workflow::create` always calls `context.ensure_mux_running()` and builds a `MuxHandle`. This refactor makes the mux layer optional so worktrees can be created and agents spawned as background processes.

This is the foundational change that enables:
- `workmux group` (#1) working headless
- Neuromancer (#9) using workmux on a mac-mini with no display
- Initiative daemon (#5) spawning tasks as background agents

## What to Change

### 1. Add headless detection

In `src/workflow/context.rs` (or wherever `WorkflowContext` is built), add logic to detect when no multiplexer is available. The existing `multiplexer::detect_backend()` already handles this -- when no mux is found, it falls back. We need a clean "headless" path when detection fails or when explicitly requested.

Add a `--headless` flag to `workmux add` in `src/cli.rs`, and also auto-detect headless when no mux is running.

### 2. Make MuxHandle optional in create workflow

In `src/workflow/create.rs`:

- Skip `context.ensure_mux_running()` when headless
- Skip `MuxHandle::new()` and all target existence checks when headless
- Skip `setup_environment()`'s window/pane creation when headless
- Still do: create git worktree, write prompt file, run post_create hooks, run file ops

The `CreateResult` should still return the worktree path and handle even in headless mode.

### 3. Headless agent spawning

When headless, the agent needs to run as a background process instead of in a tmux pane:

- Spawn `pi -p "$(cat PROMPT.md)"` (or the configured agent command) as a detached child process
- Redirect stdout/stderr to a log file in the worktree (e.g., `.workmux/agent.log`)
- Track the PID in the state store so `workmux status` can report on it
- The workmux-status extension already writes to the state store -- this should work without changes

### 4. Update setup_environment

In `src/workflow/setup.rs`, `setup_environment()` currently takes `mux: &dyn Multiplexer` as a required parameter. Options:

- Make `mux` optional (`Option<&dyn Multiplexer>`)
- Or split into `setup_environment_headless()` that does hooks + file ops without pane creation
- The second is cleaner -- less conditional logic in the hot path

### 5. State tracking for headless agents

The state store (`src/state/`) needs to track headless agents:
- Store PID + log file path alongside the existing agent status fields
- `workmux status` should show headless agents (maybe with a different indicator)
- `workmux capture` for headless agents should read from the log file instead of tmux capture-pane

### 6. Update merge and remove for headless

- `workflow::merge` and `workflow::remove` skip window cleanup when headless
- They still do: git merge, worktree removal, branch deletion, state cleanup
- Kill the agent process (by PID) if it's still running during remove

## What NOT to Change

- Don't change the interactive (mux) path at all. Existing behavior must be identical.
- Don't add the group feature. This is just the headless plumbing.
- Don't add the daemon socket. That's Phase 2.
- Don't change the CLI UX for interactive users.

## Testing

- Existing tests must pass (no regression in mux path)
- New test: create a worktree with headless mode, verify worktree exists, prompt file written, state registered
- New test: headless agent process spawns and PID is tracked
- New test: remove headless worktree cleans up properly

## Key Files

- `src/workflow/create.rs` -- main create logic, MuxHandle coupling
- `src/workflow/setup.rs` -- setup_environment, pane creation
- `src/workflow/context.rs` -- WorkflowContext, mux detection
- `src/workflow/types.rs` -- CreateArgs, SetupOptions, CreateResult
- `src/workflow/remove.rs` -- cleanup logic
- `src/workflow/merge.rs` -- merge + cleanup
- `src/cli.rs` -- add --headless flag
- `src/command/add.rs` -- passes options to workflow::create
- `src/state/` -- state store for agent tracking
- `src/multiplexer/` -- backend detection

## Reference

The existing `create()` function in `src/workflow/create.rs` is ~300 lines. The mux coupling points are:
1. `context.ensure_mux_running()` (line ~78)
2. `MuxHandle::new()` (line ~90)
3. Target existence check via `target.exists()` (line ~91)
4. Cross-repo collision detection using mux targets (line ~100-130)
5. `setup_environment()` call with mux parameter (line ~437)

All of these need conditional skipping when headless. The git worktree creation (lines ~200-280) and file operations are mux-independent and should work as-is.
