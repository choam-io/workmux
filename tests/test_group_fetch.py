"""Tests for pre-creation fetch behavior in `workmux group add`.

Verifies that `workmux group add` fetches all repos from origin in parallel
before creating worktrees, and that new branches are based on the remote
default branch rather than the local current branch.
"""

import subprocess
from pathlib import Path

import pytest
import yaml

from .conftest import (
    MuxEnvironment,
    get_scripts_dir,
    poll_until_file_has_content,
    setup_git_repo,
    WorkmuxCommandResult,
)


def get_commit_sha(env: MuxEnvironment, path: Path, ref: str = "HEAD") -> str:
    """Get the commit SHA for a ref in a repo."""
    result = env.run_command(["git", "rev-parse", ref], cwd=path)
    return result.stdout.strip()


def create_repo_with_remote(env: MuxEnvironment, name: str) -> tuple[Path, Path]:
    """Create a repo with a bare remote. Returns (repo_path, remote_path)."""
    repo_path = env.tmp_path / f"repos/{name}"
    repo_path.mkdir(parents=True)
    setup_git_repo(repo_path, env_vars=env.env)

    remote_path = env.tmp_path / f"remotes/{name}.git"
    remote_path.mkdir(parents=True)
    env.run_command(["git", "init", "--bare"], cwd=remote_path)
    # Set the bare remote's HEAD to main so clones default to main
    env.run_command(
        ["git", "symbolic-ref", "HEAD", "refs/heads/main"], cwd=remote_path
    )
    env.run_command(
        ["git", "remote", "add", "origin", str(remote_path)], cwd=repo_path
    )
    env.run_command(["git", "push", "-u", "origin", "main"], cwd=repo_path)
    return repo_path, remote_path


def advance_remote(
    env: MuxEnvironment, remote_path: Path, name: str
) -> str:
    """Clone the remote, make a commit, push. Returns the new SHA."""
    clone_path = env.tmp_path / f"clones/{name}"
    clone_path.parent.mkdir(parents=True, exist_ok=True)
    env.run_command(["git", "clone", str(remote_path), str(clone_path)])
    env.run_command(["git", "config", "user.name", "Test"], cwd=clone_path)
    env.run_command(
        ["git", "config", "user.email", "test@test.com"], cwd=clone_path
    )
    (clone_path / f"{name}_change.txt").write_text("advanced")
    env.run_command(["git", "add", "."], cwd=clone_path)
    env.run_command(
        ["git", "commit", "-m", f"Advance {name}"], cwd=clone_path
    )
    env.run_command(["git", "push"], cwd=clone_path)
    return get_commit_sha(env, clone_path)


def write_group_config(env: MuxEnvironment, group_name: str, repo_paths: list[Path]):
    """Write a workmux global config with a group definition."""
    config_dir = env.home_path / ".config" / "workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    config = {
        "nerdfont": False,
        "groups": {
            group_name: {
                "repos": [{"path": str(p)} for p in repo_paths],
            }
        },
    }
    (config_dir / "config.yaml").write_text(yaml.dump(config))


def run_group_add(
    env: MuxEnvironment,
    workmux_exe: Path,
    group_name: str,
    branch: str,
    extra_args: str = "",
    expect_fail: bool = False,
) -> WorkmuxCommandResult:
    """Run workmux group add inside the mux session and return result."""
    import shlex

    scripts_dir = get_scripts_dir(env)
    stdout_file = scripts_dir / "wm_stdout.txt"
    stderr_file = scripts_dir / "wm_stderr.txt"
    exit_code_file = scripts_dir / "wm_exit.txt"
    script_file = scripts_dir / "wm_run.sh"

    for f in [stdout_file, stderr_file, exit_code_file]:
        if f.exists():
            f.unlink()

    cmd = f"group add {group_name} {branch}"
    if extra_args:
        cmd = f"{cmd} {extra_args}"

    script_content = f"""#!/bin/sh
trap 'echo $? > {shlex.quote(str(exit_code_file))}' EXIT
export PATH={shlex.quote(env.env["PATH"])}
export TMPDIR={shlex.quote(env.env.get("TMPDIR", "/tmp"))}
export HOME={shlex.quote(env.env.get("HOME", ""))}
export WORKMUX_TEST=1
{shlex.quote(str(workmux_exe))} {cmd} > {shlex.quote(str(stdout_file))} 2> {shlex.quote(str(stderr_file))}
"""
    script_file.write_text(script_content)
    script_file.chmod(0o755)

    env.send_keys("test:", str(script_file), enter=True)

    if not poll_until_file_has_content(exit_code_file, timeout=30.0):
        pane_content = env.capture_pane("test") or "(empty)"
        raise AssertionError(
            f"group add did not complete in time\nPane:\n{pane_content}"
        )

    result = WorkmuxCommandResult(
        exit_code=int(exit_code_file.read_text().strip()),
        stdout=stdout_file.read_text() if stdout_file.exists() else "",
        stderr=stderr_file.read_text() if stderr_file.exists() else "",
    )

    if expect_fail and result.exit_code == 0:
        raise AssertionError(
            f"group add was expected to fail but succeeded.\n{result.stdout}"
        )
    if not expect_fail and result.exit_code != 0:
        raise AssertionError(
            f"group add failed (exit {result.exit_code}).\n{result.stderr}"
        )

    return result


class TestGroupFetch:
    """Tests for fetch behavior in workmux group add."""

    def test_group_add_fetches_and_bases_on_origin(
        self, mux_server: MuxEnvironment, workmux_exe_path
    ):
        """Group add should fetch all repos and base branches on origin/main."""
        env = mux_server

        repo1, remote1 = create_repo_with_remote(env, "repo1")
        repo2, remote2 = create_repo_with_remote(env, "repo2")

        # Advance both remotes ahead of local
        remote1_sha = advance_remote(env, remote1, "repo1")
        remote2_sha = advance_remote(env, remote2, "repo2")

        # Local repos are behind
        local1_sha = get_commit_sha(env, repo1, "main")
        local2_sha = get_commit_sha(env, repo2, "main")
        assert local1_sha != remote1_sha
        assert local2_sha != remote2_sha

        write_group_config(env, "test", [repo1, repo2])

        run_group_add(env, workmux_exe_path, "test", "feat/fetched")

        # Check that worktrees are based on the remote state
        wt1 = repo1.parent / "repo1__worktrees" / "feat-fetched"
        wt2 = repo2.parent / "repo2__worktrees" / "feat-fetched"
        assert wt1.exists(), f"Worktree 1 not found at {wt1}"
        assert wt2.exists(), f"Worktree 2 not found at {wt2}"

        wt1_sha = get_commit_sha(env, wt1)
        wt2_sha = get_commit_sha(env, wt2)
        assert wt1_sha == remote1_sha, (
            f"repo1 worktree at {wt1_sha}, expected {remote1_sha}"
        )
        assert wt2_sha == remote2_sha, (
            f"repo2 worktree at {wt2_sha}, expected {remote2_sha}"
        )

    def test_group_add_no_fetch_uses_local(
        self, mux_server: MuxEnvironment, workmux_exe_path
    ):
        """With --no-fetch, worktrees should be based on local state."""
        env = mux_server

        repo1, remote1 = create_repo_with_remote(env, "repo1")
        remote1_sha = advance_remote(env, remote1, "repo1")
        local1_sha = get_commit_sha(env, repo1, "main")
        assert local1_sha != remote1_sha

        write_group_config(env, "test", [repo1])

        run_group_add(
            env, workmux_exe_path, "test", "feat/no-fetch", extra_args="--no-fetch"
        )

        wt1 = repo1.parent / "repo1__worktrees" / "feat-no-fetch"
        wt1_sha = get_commit_sha(env, wt1)
        assert wt1_sha == local1_sha, (
            f"Expected local SHA {local1_sha}, got {wt1_sha}"
        )

    def test_group_add_without_remotes_succeeds(
        self, mux_server: MuxEnvironment, workmux_exe_path
    ):
        """Repos without remotes should still work (fetch fails gracefully)."""
        env = mux_server

        # Create repos without remotes
        repo1 = env.tmp_path / "repos/repo1"
        repo1.mkdir(parents=True)
        setup_git_repo(repo1, env_vars=env.env)

        write_group_config(env, "test", [repo1])

        run_group_add(env, workmux_exe_path, "test", "feat/no-remote")

        wt = repo1.parent / "repo1__worktrees" / "feat-no-remote"
        assert wt.exists()

    def test_group_add_uses_default_branch_not_current(
        self, mux_server: MuxEnvironment, workmux_exe_path
    ):
        """Group add should use default branch, not whatever branch is checked out."""
        env = mux_server

        repo1, remote1 = create_repo_with_remote(env, "repo1")

        # Create a side branch and check it out in the main worktree
        env.run_command(["git", "checkout", "-b", "some-other-branch"], cwd=repo1)
        (repo1 / "side.txt").write_text("side branch")
        env.run_command(["git", "add", "."], cwd=repo1)
        env.run_command(
            ["git", "commit", "-m", "Side branch commit"], cwd=repo1
        )

        # Advance the remote main
        remote_main_sha = advance_remote(env, remote1, "repo1")

        # Current branch in repo1 is "some-other-branch", not "main"
        result = env.run_command(
            ["git", "branch", "--show-current"], cwd=repo1
        )
        assert result.stdout.strip() == "some-other-branch"

        write_group_config(env, "test", [repo1])
        run_group_add(env, workmux_exe_path, "test", "feat/default-base")

        wt = repo1.parent / "repo1__worktrees" / "feat-default-base"
        wt_sha = get_commit_sha(env, wt)
        # Should be based on origin/main, NOT some-other-branch
        assert wt_sha == remote_main_sha, (
            f"Expected origin/main SHA {remote_main_sha}, got {wt_sha}"
        )
