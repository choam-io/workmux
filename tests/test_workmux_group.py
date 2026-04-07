"""Tests for workmux group subcommand.

Tests cross-repo worktree group functionality including:
- Group add with background mode
- Group list
- Group status
- Group remove
"""

import json
import os
import subprocess
from pathlib import Path

import pytest
import yaml


class TestGroupAdd:
    """Tests for workmux group add command."""

    def test_group_add_background_creates_workspace(self, test_env):
        """Group add in background mode creates workspace with symlinks."""
        # Create two repos
        repo1 = test_env.create_repo("repo1")
        repo2 = test_env.create_repo("repo2")

        # Configure groups in global config
        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test-group": {
                            "repos": [
                                {"path": str(repo1)},
                                {"path": str(repo2)},
                            ],
                        }
                    }
                }
            )
        )

        # Run group add in background mode
        result = test_env.run_workmux("group", "add", "test-group", "feat/test", "--background")
        assert result.returncode == 0
        assert "Created group workspace" in result.stdout

        # Verify workspace created
        ws_dir = test_env.groups_dir / "test-group--feat-test"
        assert ws_dir.exists()

        # Verify symlinks exist
        assert (ws_dir / "repo1").is_symlink()
        assert (ws_dir / "repo2").is_symlink()

        # Verify state file
        state_file = ws_dir / ".workmux-group.yaml"
        assert state_file.exists()
        state = yaml.safe_load(state_file.read_text())
        assert state["group_name"] == "test-group"
        assert state["branch"] == "feat/test"
        assert len(state["repos"]) == 2

    def test_group_add_creates_worktrees(self, test_env):
        """Group add creates worktrees in each repo."""
        repo1 = test_env.create_repo("repo1")
        repo2 = test_env.create_repo("repo2")

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [
                                {"path": str(repo1)},
                                {"path": str(repo2)},
                            ]
                        }
                    }
                }
            )
        )

        test_env.run_workmux("group", "add", "test", "feat/branch", "--background")

        # Verify worktrees created
        wt1 = repo1.parent / "repo1__worktrees" / "feat-branch"
        wt2 = repo2.parent / "repo2__worktrees" / "feat-branch"
        assert wt1.exists()
        assert wt2.exists()

    def test_group_add_fails_if_workspace_exists(self, test_env):
        """Group add fails if workspace already exists."""
        repo1 = test_env.create_repo("repo1")

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {"repos": [{"path": str(repo1)}]}
                    }
                }
            )
        )

        # First add succeeds
        result1 = test_env.run_workmux("group", "add", "test", "feat/dup", "--background")
        assert result1.returncode == 0

        # Second add fails
        result2 = test_env.run_workmux("group", "add", "test", "feat/dup", "--background")
        assert result2.returncode != 0
        assert "already exists" in result2.stderr

    def test_group_add_unknown_group_fails(self, test_env):
        """Group add fails with helpful error for unknown group."""
        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(yaml.dump({"groups": {}}))

        result = test_env.run_workmux("group", "add", "nonexistent", "feat/x", "--background")
        assert result.returncode != 0
        assert "not found in config" in result.stderr


class TestGroupList:
    """Tests for workmux group list command."""

    def test_group_list_empty(self, test_env):
        """Group list shows message when no groups."""
        result = test_env.run_workmux("group", "list")
        assert result.returncode == 0
        assert "No active group workspaces" in result.stdout

    def test_group_list_shows_groups(self, test_env):
        """Group list shows active group workspaces."""
        repo = test_env.create_repo("repo")
        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump({"groups": {"test": {"repos": [{"path": str(repo)}]}}})
        )

        test_env.run_workmux("group", "add", "test", "feat/list-test", "--background")

        result = test_env.run_workmux("group", "list")
        assert result.returncode == 0
        assert "test" in result.stdout
        assert "feat/list-test" in result.stdout

    def test_group_list_json(self, test_env):
        """Group list with --json outputs JSON."""
        repo = test_env.create_repo("repo")
        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump({"groups": {"test": {"repos": [{"path": str(repo)}]}}})
        )

        test_env.run_workmux("group", "add", "test", "feat/json", "--background")

        result = test_env.run_workmux("group", "list", "--json")
        assert result.returncode == 0
        data = json.loads(result.stdout)
        assert len(data) == 1
        assert data[0]["group_name"] == "test"
        assert data[0]["branch"] == "feat/json"


class TestGroupStatus:
    """Tests for workmux group status command."""

    def test_group_status_shows_repos(self, test_env):
        """Group status shows status for each repo."""
        repo1 = test_env.create_repo("repo1")
        repo2 = test_env.create_repo("repo2")

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [
                                {"path": str(repo1)},
                                {"path": str(repo2)},
                            ]
                        }
                    }
                }
            )
        )

        test_env.run_workmux("group", "add", "test", "feat/status", "--background")

        result = test_env.run_workmux("group", "status", "test", "feat/status")
        assert result.returncode == 0
        assert "repo1" in result.stdout
        assert "repo2" in result.stdout
        # Agent may show as "running" or "stopped" depending on whether
        # the mux backend is available in the test environment
        assert "Agent:" in result.stdout

    def test_group_status_json(self, test_env):
        """Group status with --json outputs JSON."""
        repo = test_env.create_repo("repo")
        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump({"groups": {"test": {"repos": [{"path": str(repo)}]}}})
        )

        test_env.run_workmux("group", "add", "test", "feat/status-json", "--background")

        result = test_env.run_workmux("group", "status", "test", "feat/status-json", "--json")
        assert result.returncode == 0
        data = json.loads(result.stdout)
        assert data["group_name"] == "test"
        assert data["branch"] == "feat/status-json"
        assert len(data["repos"]) == 1

    def test_group_status_not_found(self, test_env):
        """Group status fails for nonexistent workspace."""
        result = test_env.run_workmux("group", "status", "fake", "fake/branch")
        assert result.returncode != 0
        assert "not found" in result.stderr


class TestGroupRemove:
    """Tests for workmux group remove command."""

    def test_group_remove_cleans_up(self, test_env):
        """Group remove cleans up workspace and worktrees."""
        repo = test_env.create_repo("repo")
        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump({"groups": {"test": {"repos": [{"path": str(repo)}]}}})
        )

        test_env.run_workmux("group", "add", "test", "feat/remove", "--background")

        ws_dir = test_env.groups_dir / "test--feat-remove"
        wt_dir = repo.parent / "repo__worktrees" / "feat-remove"
        assert ws_dir.exists()
        assert wt_dir.exists()

        result = test_env.run_workmux("group", "remove", "test", "feat/remove", "-f")
        assert result.returncode == 0

        assert not ws_dir.exists()
        assert not wt_dir.exists()

    def test_group_remove_force_ignores_changes(self, test_env):
        """Group remove -f ignores uncommitted changes."""
        repo = test_env.create_repo("repo")
        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump({"groups": {"test": {"repos": [{"path": str(repo)}]}}})
        )

        test_env.run_workmux("group", "add", "test", "feat/dirty", "--background")

        # Create uncommitted change
        wt_dir = repo.parent / "repo__worktrees" / "feat-dirty"
        (wt_dir / "dirty.txt").write_text("uncommitted")

        result = test_env.run_workmux("group", "remove", "test", "feat/dirty", "-f")
        assert result.returncode == 0


# =============================================================================
# Fixtures
# =============================================================================


@pytest.fixture
def test_env(tmp_path):
    """Create a test environment with isolated HOME and config."""
    return GroupTestEnv(tmp_path)


class GroupTestEnv:
    """Test environment for group tests."""

    def __init__(self, tmp_path: Path):
        self.tmp_path = tmp_path
        self.home = tmp_path / "home"
        self.home.mkdir()
        self.groups_dir = self.home / ".local" / "share" / "workmux" / "groups"
        self.global_config_path = self.home / ".config" / "workmux" / "config.yaml"

        # Set HOME for workmux but preserve rustup/cargo paths
        self.env = os.environ.copy()
        self.env["HOME"] = str(self.home)
        # Preserve real home for rustup
        self.real_home = os.environ.get("HOME", str(Path.home()))

    def create_repo(self, name: str) -> Path:
        """Create a git repo with an initial commit."""
        repo = self.tmp_path / name
        repo.mkdir()

        subprocess.run(["git", "init"], cwd=repo, check=True, capture_output=True)
        subprocess.run(
            ["git", "config", "user.email", "test@test.com"],
            cwd=repo,
            check=True,
            capture_output=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "Test"],
            cwd=repo,
            check=True,
            capture_output=True,
        )

        # Create initial commit
        (repo / "README.md").write_text("# Test")
        subprocess.run(["git", "add", "."], cwd=repo, check=True, capture_output=True)
        subprocess.run(
            ["git", "commit", "-m", "Initial commit"],
            cwd=repo,
            check=True,
            capture_output=True,
        )

        return repo

    def run_workmux(self, *args) -> subprocess.CompletedProcess:
        """Run workmux with the test environment."""
        # Use the pre-built binary directly to avoid cargo issues with modified HOME
        workmux_dir = Path(__file__).parent.parent
        binary = workmux_dir / "target" / "debug" / "workmux"

        # Build if not exists (using real home for rustup)
        if not binary.exists():
            real_env = os.environ.copy()
            subprocess.run(
                ["cargo", "build"],
                cwd=workmux_dir,
                env=real_env,
                check=True,
                capture_output=True,
            )

        return subprocess.run(
            [str(binary)] + list(args),
            cwd=workmux_dir,
            env=self.env,
            capture_output=True,
            text=True,
        )


class TestGroupDirs:
    """Tests for non-git directory linking in groups."""

    def test_group_add_links_existing_dirs(self, test_env):
        """Group add symlinks non-git directories into the workspace."""
        repo = test_env.create_repo("repo1")

        # Create plain directories (not git repos)
        brain_dir = test_env.tmp_path / "brain"
        brain_dir.mkdir()
        (brain_dir / "INDEX.md").write_text("# Brain")

        notes_dir = test_env.tmp_path / "notes"
        notes_dir.mkdir()
        (notes_dir / "daily.md").write_text("# Notes")

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [{"path": str(repo)}],
                            "dirs": [str(brain_dir), str(notes_dir)],
                        }
                    }
                }
            )
        )

        result = test_env.run_workmux("group", "add", "test", "feat/dirs", "--background")
        assert result.returncode == 0
        assert "Linked dirs: 2" in result.stdout

        ws_dir = test_env.groups_dir / "test--feat-dirs"

        # Verify dir symlinks exist and point to originals
        brain_link = ws_dir / "brain"
        assert brain_link.is_symlink()
        assert (brain_link / "INDEX.md").read_text() == "# Brain"

        notes_link = ws_dir / "notes"
        assert notes_link.is_symlink()
        assert (notes_link / "daily.md").read_text() == "# Notes"

        # Verify state file tracks dirs
        state = yaml.safe_load((ws_dir / ".workmux-group.yaml").read_text())
        assert len(state["dirs"]) == 2
        dir_names = [d["symlink_name"] for d in state["dirs"]]
        assert "brain" in dir_names
        assert "notes" in dir_names

    def test_group_add_skips_missing_dirs(self, test_env):
        """Group add silently skips directories that don't exist."""
        repo = test_env.create_repo("repo1")

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [{"path": str(repo)}],
                            "dirs": ["/nonexistent/path/brain"],
                        }
                    }
                }
            )
        )

        result = test_env.run_workmux("group", "add", "test", "feat/missing-dir", "--background")
        assert result.returncode == 0

        ws_dir = test_env.groups_dir / "test--feat-missing-dir"
        assert ws_dir.exists()

        # Dir should not be linked
        assert not (ws_dir / "brain").exists()

        # State should have empty dirs
        state = yaml.safe_load((ws_dir / ".workmux-group.yaml").read_text())
        assert state.get("dirs", []) == [] or "dirs" not in state

    def test_group_add_no_dirs_field(self, test_env):
        """Group add works fine without dirs field (backward compat)."""
        repo = test_env.create_repo("repo1")

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [{"path": str(repo)}],
                        }
                    }
                }
            )
        )

        result = test_env.run_workmux("group", "add", "test", "feat/no-dirs", "--background")
        assert result.returncode == 0
        assert "Linked dirs" not in result.stdout

    def test_group_remove_cleans_up_dir_symlinks(self, test_env):
        """Group remove cleans up dir symlinks along with the workspace."""
        repo = test_env.create_repo("repo1")

        brain_dir = test_env.tmp_path / "brain"
        brain_dir.mkdir()
        (brain_dir / "INDEX.md").write_text("# Brain")

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [{"path": str(repo)}],
                            "dirs": [str(brain_dir)],
                        }
                    }
                }
            )
        )

        test_env.run_workmux("group", "add", "test", "feat/rm-dirs", "--background")
        ws_dir = test_env.groups_dir / "test--feat-rm-dirs"
        assert (ws_dir / "brain").is_symlink()

        result = test_env.run_workmux("group", "remove", "test", "feat/rm-dirs", "-f")
        assert result.returncode == 0

        # Workspace gone
        assert not ws_dir.exists()
        # Original dir untouched
        assert brain_dir.exists()
        assert (brain_dir / "INDEX.md").read_text() == "# Brain"

    def test_group_status_shows_dirs(self, test_env):
        """Group status includes linked directories."""
        repo = test_env.create_repo("repo1")

        brain_dir = test_env.tmp_path / "brain"
        brain_dir.mkdir()

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [{"path": str(repo)}],
                            "dirs": [str(brain_dir)],
                        }
                    }
                }
            )
        )

        test_env.run_workmux("group", "add", "test", "feat/status-dirs", "--background")

        result = test_env.run_workmux("group", "status", "test", "feat/status-dirs")
        assert result.returncode == 0
        assert "Linked directories:" in result.stdout
        assert "brain" in result.stdout

    def test_group_status_json_includes_dirs(self, test_env):
        """Group status --json includes dirs array."""
        repo = test_env.create_repo("repo1")

        brain_dir = test_env.tmp_path / "brain"
        brain_dir.mkdir()

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [{"path": str(repo)}],
                            "dirs": [str(brain_dir)],
                        }
                    }
                }
            )
        )

        test_env.run_workmux("group", "add", "test", "feat/json-dirs", "--background")

        result = test_env.run_workmux("group", "status", "test", "feat/json-dirs", "--json")
        assert result.returncode == 0
        data = json.loads(result.stdout)
        assert len(data["dirs"]) == 1
        assert data["dirs"][0]["name"] == "brain"
        assert data["dirs"][0]["exists"] is True

    def test_group_add_dir_name_conflict_with_repo(self, test_env):
        """Dir with same basename as a repo is skipped to avoid symlink conflict."""
        repo = test_env.create_repo("myrepo")

        # Create a dir with the same name as the repo
        conflict_dir = test_env.tmp_path / "myrepo_dir"
        conflict_dir.mkdir()
        # Rename to match -- we can't have two things named "myrepo"
        # Actually the repo symlink uses the repo dir name, and the dir symlink uses the dir basename
        # So let's create a dir literally named "myrepo" elsewhere
        myrepo_dir = test_env.tmp_path / "other" / "myrepo"
        myrepo_dir.mkdir(parents=True)

        config = test_env.global_config_path
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(
            yaml.dump(
                {
                    "groups": {
                        "test": {
                            "repos": [{"path": str(repo)}],
                            "dirs": [str(myrepo_dir)],
                        }
                    }
                }
            )
        )

        result = test_env.run_workmux("group", "add", "test", "feat/conflict", "--background")
        assert result.returncode == 0

        ws_dir = test_env.groups_dir / "test--feat-conflict"
        # The repo symlink should exist (created first)
        assert (ws_dir / "myrepo").is_symlink()

        # The dir should be skipped (conflict)
        state = yaml.safe_load((ws_dir / ".workmux-group.yaml").read_text())
        assert len(state.get("dirs", [])) == 0
