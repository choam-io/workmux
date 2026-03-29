"""Tests for `workmux group` subcommand - cross-repo worktree management."""

import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest
import yaml

from .conftest import MuxEnvironment, setup_git_repo


def create_multi_repo_setup(
    mux_server: MuxEnvironment, repo_names: list[str]
) -> tuple[Path, dict[str, Path]]:
    """Create multiple git repos in a parent directory.
    
    Returns:
        Tuple of (parent_dir, {repo_name: repo_path})
    """
    parent_dir = mux_server.tmp_path / "repos"
    parent_dir.mkdir(parents=True, exist_ok=True)
    
    repos = {}
    for name in repo_names:
        repo_path = parent_dir / name
        repo_path.mkdir()
        setup_git_repo(repo_path)
        # Create a test file
        (repo_path / "README.md").write_text(f"# {name}\n")
        subprocess.run(
            ["git", "add", "README.md"],
            cwd=repo_path,
            check=True,
            capture_output=True,
        )
        subprocess.run(
            ["git", "commit", "-m", "Initial commit"],
            cwd=repo_path,
            check=True,
            capture_output=True,
        )
        repos[name] = repo_path
    
    return parent_dir, repos


def write_global_config_with_group(
    mux_server: MuxEnvironment,
    group_name: str,
    repo_paths: list[Path],
    merge_order: list[str] | None = None,
):
    """Write global config with a group definition."""
    config_dir = mux_server.home_path / ".config" / "workmux"
    config_dir.mkdir(parents=True, exist_ok=True)
    
    group_config = {
        "groups": {
            group_name: {
                "repos": [{"path": str(p)} for p in repo_paths],
            }
        }
    }
    
    if merge_order:
        group_config["groups"][group_name]["merge_order"] = merge_order
    
    config_path = config_dir / "config.yaml"
    config_path.write_text(yaml.dump(group_config))
    return config_path


# =============================================================================
# Tests: group list
# =============================================================================


def test_group_list_empty(mux_server: MuxEnvironment, workmux_exe_path: Path):
    """Verifies `workmux group list` shows no groups when none exist."""
    result = mux_server.run_command([str(workmux_exe_path), "group", "list"])
    
    assert result.returncode == 0
    assert "No active group workspaces" in result.stdout


def test_group_list_json_empty(mux_server: MuxEnvironment, workmux_exe_path: Path):
    """Verifies `workmux group list --json` returns empty array."""
    result = mux_server.run_command([str(workmux_exe_path), "group", "list", "--json"])
    
    assert result.returncode == 0
    assert result.stdout.strip() == "[]"


# =============================================================================
# Tests: group add
# =============================================================================


def test_group_add_missing_group_config(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group add` fails when group is not defined in config."""
    with pytest.raises(subprocess.CalledProcessError) as exc_info:
        mux_server.run_command(
            [str(workmux_exe_path), "group", "add", "undefined-group", "feat/test"]
        )
    
    assert exc_info.value.returncode != 0
    assert "not found in config" in exc_info.value.stderr


def test_group_add_headless_creates_worktrees(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group add --headless` creates worktrees across repos."""
    parent_dir, repos = create_multi_repo_setup(mux_server, ["repo-a", "repo-b"])
    
    write_global_config_with_group(
        mux_server,
        "test-group",
        [repos["repo-a"], repos["repo-b"]],
    )
    
    result = mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "test-group", "feat/new-feature", "--headless"]
    )
    
    assert result.returncode == 0
    assert "Created group workspace" in result.stdout
    assert "Repositories: 2" in result.stdout
    
    # Verify worktrees were created
    worktree_a = parent_dir / "repo-a__worktrees" / "feat-new-feature"
    worktree_b = parent_dir / "repo-b__worktrees" / "feat-new-feature"
    assert worktree_a.exists(), f"Worktree not created at {worktree_a}"
    assert worktree_b.exists(), f"Worktree not created at {worktree_b}"
    
    # Verify workspace directory with symlinks
    groups_dir = mux_server.home_path / ".local" / "share" / "workmux" / "groups"
    workspace_dir = groups_dir / "test-group--feat-new-feature"
    assert workspace_dir.exists()
    assert (workspace_dir / "repo-a").is_symlink()
    assert (workspace_dir / "repo-b").is_symlink()
    
    # Verify state file
    state_file = workspace_dir / ".workmux-group.yaml"
    assert state_file.exists()
    state = yaml.safe_load(state_file.read_text())
    assert state["group_name"] == "test-group"
    assert state["branch"] == "feat/new-feature"
    assert state["headless"] is True
    assert len(state["repos"]) == 2


def test_group_add_with_prompt_file(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group add -P` writes prompt file to workspace."""
    parent_dir, repos = create_multi_repo_setup(mux_server, ["repo-a"])
    
    write_global_config_with_group(mux_server, "single", [repos["repo-a"]])
    
    # Create a prompt file
    prompt_file = mux_server.tmp_path / "prompt.txt"
    prompt_file.write_text("Implement cross-repo feature X")
    
    result = mux_server.run_command(
        [
            str(workmux_exe_path),
            "group", "add", "single", "feat/prompted",
            "--headless",
            "-P", str(prompt_file),
        ]
    )
    
    assert result.returncode == 0
    
    # Verify prompt file was written to workspace
    groups_dir = mux_server.home_path / ".local" / "share" / "workmux" / "groups"
    workspace_dir = groups_dir / "single--feat-prompted"
    prompt_path = workspace_dir / ".workmux" / "PROMPT.md"
    assert prompt_path.exists()
    assert "Implement cross-repo feature X" in prompt_path.read_text()


def test_group_add_duplicate_fails(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies creating duplicate group workspace fails."""
    _, repos = create_multi_repo_setup(mux_server, ["repo-a"])
    write_global_config_with_group(mux_server, "test", [repos["repo-a"]])
    
    # First add succeeds
    result = mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "test", "feat/dup", "--headless"]
    )
    assert result.returncode == 0
    
    # Second add fails
    with pytest.raises(subprocess.CalledProcessError) as exc_info:
        mux_server.run_command(
            [str(workmux_exe_path), "group", "add", "test", "feat/dup", "--headless"]
        )
    
    assert exc_info.value.returncode != 0
    assert "already exists" in exc_info.value.stderr


# =============================================================================
# Tests: group list (with data)
# =============================================================================


def test_group_list_shows_active_groups(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group list` shows created groups."""
    _, repos = create_multi_repo_setup(mux_server, ["repo-a", "repo-b"])
    write_global_config_with_group(mux_server, "mygroup", list(repos.values()))
    
    # Create a group workspace
    mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "mygroup", "feat/test", "--headless"]
    )
    
    result = mux_server.run_command([str(workmux_exe_path), "group", "list"])
    
    assert result.returncode == 0
    assert "mygroup" in result.stdout
    assert "feat/test" in result.stdout
    assert "2" in result.stdout  # Repos count


def test_group_list_json(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group list --json` returns structured data."""
    _, repos = create_multi_repo_setup(mux_server, ["repo-a"])
    write_global_config_with_group(mux_server, "jsontest", list(repos.values()))
    
    mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "jsontest", "feat/json", "--headless"]
    )
    
    result = mux_server.run_command([str(workmux_exe_path), "group", "list", "--json"])
    
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert len(data) == 1
    assert data[0]["group_name"] == "jsontest"
    assert data[0]["branch"] == "feat/json"


# =============================================================================
# Tests: group status
# =============================================================================


def test_group_status_shows_repo_state(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group status` shows repository states."""
    parent_dir, repos = create_multi_repo_setup(mux_server, ["repo-a", "repo-b"])
    write_global_config_with_group(mux_server, "statustest", list(repos.values()))
    
    mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "statustest", "feat/status", "--headless"]
    )
    
    result = mux_server.run_command(
        [str(workmux_exe_path), "group", "status", "statustest", "feat/status"]
    )
    
    assert result.returncode == 0
    assert "statustest" in result.stdout
    assert "feat/status" in result.stdout
    assert "repo-a" in result.stdout
    assert "repo-b" in result.stdout
    assert "Agent:" in result.stdout


def test_group_status_json(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group status --json` returns structured data."""
    _, repos = create_multi_repo_setup(mux_server, ["repo-a"])
    write_global_config_with_group(mux_server, "jsonstat", list(repos.values()))
    
    mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "jsonstat", "feat/stat", "--headless"]
    )
    
    result = mux_server.run_command(
        [str(workmux_exe_path), "group", "status", "jsonstat", "feat/stat", "--json"]
    )
    
    assert result.returncode == 0
    data = json.loads(result.stdout)
    assert data["group_name"] == "jsonstat"
    assert data["branch"] == "feat/stat"
    assert len(data["repos"]) == 1
    assert data["repos"][0]["worktree_exists"] is True


def test_group_status_not_found(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group status` fails for non-existent group."""
    with pytest.raises(subprocess.CalledProcessError) as exc_info:
        mux_server.run_command(
            [str(workmux_exe_path), "group", "status", "nosuchgroup", "feat/x"]
        )
    
    assert exc_info.value.returncode != 0
    assert "not found" in exc_info.value.stderr


# =============================================================================
# Tests: group remove
# =============================================================================


def test_group_remove_cleans_up_worktrees(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group remove` removes worktrees and workspace."""
    parent_dir, repos = create_multi_repo_setup(mux_server, ["repo-a", "repo-b"])
    write_global_config_with_group(mux_server, "rmtest", list(repos.values()))
    
    mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "rmtest", "feat/remove", "--headless"]
    )
    
    # Verify worktrees exist
    worktree_a = parent_dir / "repo-a__worktrees" / "feat-remove"
    worktree_b = parent_dir / "repo-b__worktrees" / "feat-remove"
    assert worktree_a.exists()
    assert worktree_b.exists()
    
    # Remove with force flag
    result = mux_server.run_command(
        [str(workmux_exe_path), "group", "remove", "rmtest", "feat/remove", "-f"]
    )
    
    assert result.returncode == 0
    assert "Group workspace removed" in result.stdout
    
    # Verify worktrees are gone
    assert not worktree_a.exists()
    assert not worktree_b.exists()
    
    # Verify workspace directory is gone
    groups_dir = mux_server.home_path / ".local" / "share" / "workmux" / "groups"
    workspace_dir = groups_dir / "rmtest--feat-remove"
    assert not workspace_dir.exists()


def test_group_remove_not_found(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group remove` fails for non-existent group."""
    with pytest.raises(subprocess.CalledProcessError) as exc_info:
        mux_server.run_command(
            [str(workmux_exe_path), "group", "remove", "nosuchgroup", "feat/x", "-f"]
        )
    
    assert exc_info.value.returncode != 0
    assert "not found" in exc_info.value.stderr


# =============================================================================
# Tests: group merge
# =============================================================================


def test_group_merge_merges_all_repos(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `workmux group merge` merges branches across repos."""
    parent_dir, repos = create_multi_repo_setup(mux_server, ["repo-a", "repo-b"])
    write_global_config_with_group(
        mux_server,
        "mergetest",
        list(repos.values()),
        merge_order=["repo-a", "repo-b"],
    )
    
    mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "mergetest", "feat/merge", "--headless"]
    )
    
    # Make changes in the worktrees
    worktree_a = parent_dir / "repo-a__worktrees" / "feat-merge"
    worktree_b = parent_dir / "repo-b__worktrees" / "feat-merge"
    
    (worktree_a / "new-file-a.txt").write_text("content a")
    subprocess.run(["git", "add", "new-file-a.txt"], cwd=worktree_a, check=True)
    subprocess.run(["git", "commit", "-m", "Add file a"], cwd=worktree_a, check=True)
    
    (worktree_b / "new-file-b.txt").write_text("content b")
    subprocess.run(["git", "add", "new-file-b.txt"], cwd=worktree_b, check=True)
    subprocess.run(["git", "commit", "-m", "Add file b"], cwd=worktree_b, check=True)
    
    # Merge (using stdin to confirm)
    result = subprocess.run(
        [str(workmux_exe_path), "group", "merge", "mergetest", "feat/merge"],
        cwd=mux_server.tmp_path,
        env=mux_server.env,
        input="y\n",
        capture_output=True,
        text=True,
    )
    
    assert result.returncode == 0
    assert "Merged: repo-a" in result.stdout
    assert "Merged: repo-b" in result.stdout
    assert "Group merge complete" in result.stdout
    
    # Verify changes are in main branches
    assert (repos["repo-a"] / "new-file-a.txt").exists()
    assert (repos["repo-b"] / "new-file-b.txt").exists()


def test_group_merge_respects_order(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies merge order is respected."""
    parent_dir, repos = create_multi_repo_setup(mux_server, ["first", "second", "third"])
    write_global_config_with_group(
        mux_server,
        "ordertest",
        list(repos.values()),
        merge_order=["third", "first", "second"],
    )
    
    mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "ordertest", "feat/order", "--headless"]
    )
    
    # Make commits in each worktree
    for name, _ in repos.items():
        worktree = parent_dir / f"{name}__worktrees" / "feat-order"
        (worktree / f"{name}.txt").write_text(name)
        subprocess.run(["git", "add", f"{name}.txt"], cwd=worktree, check=True)
        subprocess.run(["git", "commit", "-m", f"Add {name}"], cwd=worktree, check=True)
    
    result = subprocess.run(
        [str(workmux_exe_path), "group", "merge", "ordertest", "feat/order"],
        cwd=mux_server.tmp_path,
        env=mux_server.env,
        input="y\n",
        capture_output=True,
        text=True,
    )
    
    # Verify order by checking stdout line positions
    stdout = result.stdout
    assert stdout.index("Merged: third") < stdout.index("Merged: first")
    assert stdout.index("Merged: first") < stdout.index("Merged: second")


def test_group_merge_with_keep(
    mux_server: MuxEnvironment, workmux_exe_path: Path
):
    """Verifies `--keep` preserves worktrees after merge."""
    parent_dir, repos = create_multi_repo_setup(mux_server, ["repo-a"])
    write_global_config_with_group(mux_server, "keeptest", list(repos.values()))
    
    mux_server.run_command(
        [str(workmux_exe_path), "group", "add", "keeptest", "feat/keep", "--headless"]
    )
    
    worktree = parent_dir / "repo-a__worktrees" / "feat-keep"
    (worktree / "kept.txt").write_text("kept")
    subprocess.run(["git", "add", "kept.txt"], cwd=worktree, check=True)
    subprocess.run(["git", "commit", "-m", "Keep me"], cwd=worktree, check=True)
    
    result = subprocess.run(
        [str(workmux_exe_path), "group", "merge", "keeptest", "feat/keep", "--keep"],
        cwd=mux_server.tmp_path,
        env=mux_server.env,
        input="y\n",
        capture_output=True,
        text=True,
    )
    
    assert result.returncode == 0
    
    # Workspace should still exist
    groups_dir = mux_server.home_path / ".local" / "share" / "workmux" / "groups"
    workspace_dir = groups_dir / "keeptest--feat-keep"
    assert workspace_dir.exists()
