"""Tests for pre-creation fetch behavior in `workmux add`.

Verifies that `workmux add` fetches from origin before creating worktrees,
and that new branches are based on the remote state rather than stale local state.
"""

import shlex
import subprocess
from pathlib import Path

import pytest

from ..conftest import (
    MuxEnvironment,
    WorkmuxCommandResult,
    get_scripts_dir,
    get_worktree_path,
    poll_until_file_has_content,
    write_workmux_config,
)


def get_commit_sha(env: MuxEnvironment, path: Path, ref: str = "HEAD") -> str:
    """Get the commit SHA for a ref in a repo."""
    result = env.run_command(["git", "rev-parse", ref], cwd=path)
    return result.stdout.strip()


def setup_repo_with_remote(env: MuxEnvironment, repo_path: Path):
    """Set up a bare remote, push repo to it, then advance remote ahead.

    Returns (remote_path, remote_head_sha).
    """
    remote_path = env.tmp_path / "remote_repo.git"
    remote_path.mkdir()

    # Create bare remote
    subprocess.run(["git", "init", "--bare"], cwd=remote_path, check=True,
                   capture_output=True)

    # Add remote and push
    subprocess.run(["git", "remote", "add", "origin", str(remote_path)],
                   cwd=repo_path, check=True, capture_output=True)
    subprocess.run(["git", "push", "-u", "origin", "main"],
                   cwd=repo_path, check=True, capture_output=True)

    # Clone the remote, advance it, push back
    clone_path = env.tmp_path / "clone_advance"
    subprocess.run(["git", "clone", str(remote_path), str(clone_path)],
                   check=True, capture_output=True)
    subprocess.run(["git", "config", "user.name", "Test"],
                   cwd=clone_path, check=True, capture_output=True)
    subprocess.run(["git", "config", "user.email", "test@test.com"],
                   cwd=clone_path, check=True, capture_output=True)
    (clone_path / "remote_change.txt").write_text("advanced")
    subprocess.run(["git", "add", "."], cwd=clone_path, check=True,
                   capture_output=True)
    subprocess.run(["git", "commit", "-m", "Advance remote"], cwd=clone_path,
                   check=True, capture_output=True)
    subprocess.run(["git", "push"], cwd=clone_path, check=True,
                   capture_output=True)

    remote_sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=clone_path,
        check=True, capture_output=True, text=True
    ).stdout.strip()

    return remote_path, remote_sha


def run_workmux_add_isolated(
    env: MuxEnvironment,
    workmux_exe: Path,
    repo_path: Path,
    command: str,
) -> WorkmuxCommandResult:
    """Run a workmux command inside tmux, forcing tmux backend."""
    scripts_dir = get_scripts_dir(env)
    stdout_file = scripts_dir / "wm_stdout.txt"
    stderr_file = scripts_dir / "wm_stderr.txt"
    exit_code_file = scripts_dir / "wm_exit.txt"
    script_file = scripts_dir / "wm_run.sh"

    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    script_content = f"""#!/bin/sh
trap 'echo $? > {shlex.quote(str(exit_code_file))}' EXIT
export PATH={shlex.quote(env.env["PATH"])}
export TMPDIR={shlex.quote(env.env.get("TMPDIR", "/tmp"))}
export HOME={shlex.quote(env.env.get("HOME", ""))}
export WORKMUX_TEST=1
export WORKMUX_BACKEND=tmux
unset CMUX_WORKSPACE_ID CMUX_SURFACE_ID
cd {shlex.quote(str(repo_path))}
{shlex.quote(str(workmux_exe))} {command} > {shlex.quote(str(stdout_file))} 2> {shlex.quote(str(stderr_file))}
"""
    script_file.write_text(script_content)
    script_file.chmod(0o755)

    env.send_keys("test:", str(script_file), enter=True)

    if not poll_until_file_has_content(exit_code_file, timeout=15.0):
        pane_content = env.capture_pane("test") or "(empty)"
        raise AssertionError(
            f"workmux command did not complete in time\nPane:\n{pane_content}"
        )

    result = WorkmuxCommandResult(
        exit_code=int(exit_code_file.read_text().strip()),
        stdout=stdout_file.read_text() if stdout_file.exists() else "",
        stderr=stderr_file.read_text() if stderr_file.exists() else "",
    )

    if result.exit_code != 0:
        raise AssertionError(
            f"workmux {command} failed (exit {result.exit_code})\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )

    return result


class TestFetchBeforeAdd:
    """Tests that workmux add fetches from origin and uses remote state."""

    def test_add_fetches_and_bases_on_origin(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path
    ):
        """New branch should be based on origin/main, not stale local main."""
        env = mux_server
        write_workmux_config(mux_repo_path, env=env)

        remote_path, remote_sha = setup_repo_with_remote(env, mux_repo_path)

        # Verify setup: local main is behind
        local_main_sha = get_commit_sha(env, mux_repo_path, "main")
        assert local_main_sha != remote_sha, "Setup failed: local should be behind remote"

        # Verify: manual fetch from the same process works
        subprocess.run(
            ["git", "fetch", "origin"], cwd=mux_repo_path,
            check=True, capture_output=True
        )
        after_fetch = get_commit_sha(env, mux_repo_path, "origin/main")
        assert after_fetch == remote_sha, (
            f"Manual fetch didn't work: origin/main={after_fetch}, expected={remote_sha}"
        )

        # Reset origin/main back to stale state for the real test
        subprocess.run(
            ["git", "update-ref", "refs/remotes/origin/main", local_main_sha],
            cwd=mux_repo_path, check=True, capture_output=True
        )

        # workmux add should fetch and base the new branch on origin/main
        run_workmux_add_isolated(
            env, workmux_exe_path, mux_repo_path, "add feat/after-fetch"
        )

        worktree_path = get_worktree_path(mux_repo_path, "feat/after-fetch")
        assert worktree_path.is_dir(), f"Worktree not found at {worktree_path}"

        # The new branch should be at the remote commit
        new_branch_sha = get_commit_sha(env, worktree_path, "HEAD")
        assert new_branch_sha == remote_sha, (
            f"Expected worktree HEAD at remote SHA {remote_sha}, "
            f"got {new_branch_sha} (local main was {local_main_sha})"
        )

    def test_add_no_fetch_uses_local_state(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path
    ):
        """With --no-fetch, new branch should be based on local main."""
        env = mux_server
        write_workmux_config(mux_repo_path, env=env)

        _, remote_sha = setup_repo_with_remote(env, mux_repo_path)
        local_main_sha = get_commit_sha(env, mux_repo_path, "main")
        assert local_main_sha != remote_sha

        run_workmux_add_isolated(
            env, workmux_exe_path, mux_repo_path,
            "add feat/no-fetch --no-fetch",
        )

        worktree_path = get_worktree_path(mux_repo_path, "feat/no-fetch")
        assert worktree_path.is_dir()

        new_branch_sha = get_commit_sha(env, worktree_path, "HEAD")
        assert new_branch_sha == local_main_sha, (
            f"Expected local SHA {local_main_sha}, got {new_branch_sha}"
        )

    def test_add_without_remote_succeeds(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path
    ):
        """Repos without a remote should still work (fetch fails gracefully)."""
        env = mux_server
        write_workmux_config(mux_repo_path, env=env)

        run_workmux_add_isolated(
            env, workmux_exe_path, mux_repo_path, "add feat/no-remote"
        )

        worktree_path = get_worktree_path(mux_repo_path, "feat/no-remote")
        assert worktree_path.is_dir()

        local_main_sha = get_commit_sha(env, mux_repo_path, "main")
        new_branch_sha = get_commit_sha(env, worktree_path, "HEAD")
        assert new_branch_sha == local_main_sha

    def test_add_with_explicit_base_prefers_origin(
        self, mux_server: MuxEnvironment, workmux_exe_path, mux_repo_path
    ):
        """With --base main, should still prefer origin/main after fetching."""
        env = mux_server
        write_workmux_config(mux_repo_path, env=env)

        remote_path, remote_sha = setup_repo_with_remote(env, mux_repo_path)

        # Reset origin/main to stale state
        local_main_sha = get_commit_sha(env, mux_repo_path, "main")
        subprocess.run(
            ["git", "fetch", "origin"], cwd=mux_repo_path,
            check=True, capture_output=True
        )
        subprocess.run(
            ["git", "update-ref", "refs/remotes/origin/main", local_main_sha],
            cwd=mux_repo_path, check=True, capture_output=True
        )

        run_workmux_add_isolated(
            env, workmux_exe_path, mux_repo_path,
            "add feat/explicit-base --base main",
        )

        worktree_path = get_worktree_path(mux_repo_path, "feat/explicit-base")
        assert worktree_path.is_dir()

        new_branch_sha = get_commit_sha(env, worktree_path, "HEAD")
        assert new_branch_sha == remote_sha, (
            f"Expected origin/main SHA {remote_sha}, got {new_branch_sha}"
        )
