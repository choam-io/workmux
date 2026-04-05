"""Tests for workmux group fork subcommand.

Tests fork-specific functionality:
- Fork creates worktrees from source HEAD (not default branch)
- Uncommitted changes (staged, unstaged, untracked) are copied
- Source workspace is left untouched
- workmux-base is propagated
- Duplicate target fails
- Missing source fails
"""

import json
import os
import subprocess
from pathlib import Path

import pytest
import yaml


# =============================================================================
# Test environment
# =============================================================================


class ForkTestEnv:
    """Isolated test environment for group fork tests."""

    def __init__(self, tmp_path: Path):
        self.tmp_path = tmp_path
        self.home = tmp_path / "home"
        self.home.mkdir()
        self.groups_dir = self.home / ".local" / "share" / "workmux" / "groups"
        self.global_config_path = self.home / ".config" / "workmux" / "config.yaml"

        self.env = os.environ.copy()
        self.env["HOME"] = str(self.home)

    def create_repo(self, name: str) -> Path:
        """Create a git repo with an initial commit."""
        repo = self.tmp_path / name
        repo.mkdir()

        subprocess.run(["git", "init"], cwd=repo, check=True, capture_output=True)
        subprocess.run(
            ["git", "config", "user.email", "test@test.com"],
            cwd=repo, check=True, capture_output=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "Test"],
            cwd=repo, check=True, capture_output=True,
        )
        (repo / "README.md").write_text("# Test")
        subprocess.run(["git", "add", "."], cwd=repo, check=True, capture_output=True)
        subprocess.run(
            ["git", "commit", "-m", "Initial commit"],
            cwd=repo, check=True, capture_output=True,
        )
        return repo

    def configure_group(self, group_name: str, repos: list[Path]):
        """Write group config."""
        self.global_config_path.parent.mkdir(parents=True, exist_ok=True)
        self.global_config_path.write_text(
            yaml.dump({
                "groups": {
                    group_name: {
                        "repos": [{"path": str(r)} for r in repos],
                    }
                }
            })
        )

    def run_workmux(self, *args) -> subprocess.CompletedProcess:
        """Run workmux binary with isolated HOME."""
        workmux_dir = Path(__file__).parent.parent
        binary = workmux_dir / "target" / "debug" / "workmux"

        if not binary.exists():
            real_env = os.environ.copy()
            subprocess.run(
                ["cargo", "build"], cwd=workmux_dir,
                env=real_env, check=True, capture_output=True,
            )

        return subprocess.run(
            [str(binary)] + list(args),
            cwd=workmux_dir,
            env=self.env,
            capture_output=True,
            text=True,
        )

    def group_add(self, group_name: str, branch: str):
        """Create a group workspace (background mode, no mux)."""
        result = self.run_workmux("group", "add", group_name, branch, "--background")
        assert result.returncode == 0, f"group add failed: {result.stderr}"

    def group_fork(self, group_name: str, source_branch: str, new_branch: str):
        """Fork a group workspace (background mode, no mux)."""
        result = self.run_workmux(
            "group", "fork", group_name, source_branch,
            "-b", new_branch, "--background",
        )
        return result

    def worktree_path(self, repo: Path, branch: str) -> Path:
        """Get the expected worktree path for a repo/branch."""
        slug = branch.replace("/", "-")
        return repo.parent / f"{repo.name}__worktrees" / slug

    def workspace_dir(self, group_name: str, branch: str) -> Path:
        slug = branch.replace("/", "-")
        return self.groups_dir / f"{group_name}--{slug}"


@pytest.fixture
def env(tmp_path):
    return ForkTestEnv(tmp_path)


# =============================================================================
# Tests
# =============================================================================


class TestGroupForkBasic:
    """Basic fork functionality."""

    def test_fork_creates_workspace_and_worktrees(self, env):
        """Fork creates a new workspace directory with worktrees and symlinks."""
        repo1 = env.create_repo("repo1")
        repo2 = env.create_repo("repo2")
        env.configure_group("g", [repo1, repo2])
        env.group_add("g", "feat/source")

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"
        assert "Forked group workspace" in result.stdout

        # New workspace exists
        ws = env.workspace_dir("g", "feat/forked")
        assert ws.exists()
        assert (ws / "repo1").is_symlink()
        assert (ws / "repo2").is_symlink()

        # State file is correct
        state = yaml.safe_load((ws / ".workmux-group.yaml").read_text())
        assert state["group_name"] == "g"
        assert state["branch"] == "feat/forked"
        assert len(state["repos"]) == 2

        # Worktrees exist
        assert env.worktree_path(repo1, "feat/forked").exists()
        assert env.worktree_path(repo2, "feat/forked").exists()

    def test_fork_branches_from_source_head(self, env):
        """Fork branches from the source worktree's HEAD, not from the default branch."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        # Make a commit on the source branch
        wt = env.worktree_path(repo, "feat/source")
        (wt / "new-file.txt").write_text("from source branch")
        subprocess.run(["git", "add", "."], cwd=wt, check=True, capture_output=True)
        subprocess.run(
            ["git", "commit", "-m", "source commit"],
            cwd=wt, check=True, capture_output=True,
        )

        # Get source HEAD
        source_head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=wt,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

        # Fork
        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        # Forked worktree should have the same HEAD
        fork_wt = env.worktree_path(repo, "feat/forked")
        fork_head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=fork_wt,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

        assert fork_head == source_head
        # And the file from the source commit should be there
        assert (fork_wt / "new-file.txt").exists()
        assert (fork_wt / "new-file.txt").read_text() == "from source branch"

    def test_fork_source_untouched(self, env):
        """Fork does not modify the source workspace."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        wt = env.worktree_path(repo, "feat/source")
        (wt / "dirty.txt").write_text("uncommitted")

        # Snapshot source state
        source_status_before = subprocess.run(
            ["git", "status", "--porcelain"], cwd=wt,
            check=True, capture_output=True, text=True,
        ).stdout

        env.group_fork("g", "feat/source", "feat/forked")

        # Source state unchanged
        source_status_after = subprocess.run(
            ["git", "status", "--porcelain"], cwd=wt,
            check=True, capture_output=True, text=True,
        ).stdout
        assert source_status_before == source_status_after
        assert (wt / "dirty.txt").read_text() == "uncommitted"


class TestGroupForkDirtyState:
    """Dirty state transfer: staged, unstaged, untracked."""

    def test_fork_copies_untracked_files(self, env):
        """Untracked files are copied to the forked workspace."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        wt = env.worktree_path(repo, "feat/source")
        (wt / "untracked.txt").write_text("hello untracked")
        (wt / "subdir").mkdir()
        (wt / "subdir" / "nested.txt").write_text("nested untracked")

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt = env.worktree_path(repo, "feat/forked")
        assert (fork_wt / "untracked.txt").read_text() == "hello untracked"
        assert (fork_wt / "subdir" / "nested.txt").read_text() == "nested untracked"

    def test_fork_copies_staged_changes(self, env):
        """Staged (cached) changes are copied to the forked workspace."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        wt = env.worktree_path(repo, "feat/source")
        (wt / "staged.txt").write_text("staged content")
        subprocess.run(["git", "add", "staged.txt"], cwd=wt, check=True, capture_output=True)

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt = env.worktree_path(repo, "feat/forked")
        assert (fork_wt / "staged.txt").read_text() == "staged content"

        # Verify it's staged in the fork too
        staged_check = subprocess.run(
            ["git", "diff", "--cached", "--name-only"], cwd=fork_wt,
            check=True, capture_output=True, text=True,
        )
        assert "staged.txt" in staged_check.stdout

    def test_fork_copies_unstaged_changes(self, env):
        """Unstaged modifications to tracked files are copied."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        wt = env.worktree_path(repo, "feat/source")
        # Modify existing tracked file
        (wt / "README.md").write_text("modified content")

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt = env.worktree_path(repo, "feat/forked")
        assert (fork_wt / "README.md").read_text() == "modified content"

        # Verify it's unstaged in the fork
        unstaged_check = subprocess.run(
            ["git", "diff", "--name-only"], cwd=fork_wt,
            check=True, capture_output=True, text=True,
        )
        assert "README.md" in unstaged_check.stdout

    def test_fork_copies_mixed_dirty_state(self, env):
        """Fork handles staged + unstaged + untracked simultaneously."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        wt = env.worktree_path(repo, "feat/source")

        # Staged new file
        (wt / "staged.txt").write_text("staged")
        subprocess.run(["git", "add", "staged.txt"], cwd=wt, check=True, capture_output=True)

        # Unstaged modification
        (wt / "README.md").write_text("modified")

        # Untracked
        (wt / "untracked.txt").write_text("untracked")

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt = env.worktree_path(repo, "feat/forked")
        assert (fork_wt / "staged.txt").read_text() == "staged"
        assert (fork_wt / "README.md").read_text() == "modified"
        assert (fork_wt / "untracked.txt").read_text() == "untracked"

    def test_fork_clean_source_works(self, env):
        """Fork works fine when source has no dirty state."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt = env.worktree_path(repo, "feat/forked")
        status = subprocess.run(
            ["git", "status", "--porcelain"], cwd=fork_wt,
            check=True, capture_output=True, text=True,
        )
        assert status.stdout.strip() == ""

    def test_fork_copies_staged_deletion(self, env):
        """Staged deletions (git rm) are correctly transferred."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        wt = env.worktree_path(repo, "feat/source")
        # Stage a deletion of the tracked README.md
        subprocess.run(["git", "rm", "README.md"], cwd=wt, check=True, capture_output=True)

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt = env.worktree_path(repo, "feat/forked")
        # File should not exist on disk
        assert not (fork_wt / "README.md").exists()
        # And should be staged as deleted
        staged = subprocess.run(
            ["git", "diff", "--cached", "--name-only", "--diff-filter=D"], cwd=fork_wt,
            check=True, capture_output=True, text=True,
        )
        assert "README.md" in staged.stdout

    def test_fork_copies_staged_modification(self, env):
        """Staged modification of an existing tracked file is transferred."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        wt = env.worktree_path(repo, "feat/source")
        (wt / "README.md").write_text("staged modification")
        subprocess.run(["git", "add", "README.md"], cwd=wt, check=True, capture_output=True)

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt = env.worktree_path(repo, "feat/forked")
        assert (fork_wt / "README.md").read_text() == "staged modification"
        # Should be staged
        staged = subprocess.run(
            ["git", "diff", "--cached", "--name-only"], cwd=fork_wt,
            check=True, capture_output=True, text=True,
        )
        assert "README.md" in staged.stdout

    def test_fork_copies_binary_file(self, env):
        """Binary files in dirty state are transferred."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        wt = env.worktree_path(repo, "feat/source")
        # Write a binary file (null bytes)
        binary_content = bytes(range(256))
        (wt / "binary.bin").write_bytes(binary_content)

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt = env.worktree_path(repo, "feat/forked")
        assert (fork_wt / "binary.bin").read_bytes() == binary_content


class TestGroupForkErrors:
    """Error cases."""

    def test_fork_fails_if_target_exists(self, env):
        """Fork fails if the target workspace already exists."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        result1 = env.group_fork("g", "feat/source", "feat/forked")
        assert result1.returncode == 0

        result2 = env.group_fork("g", "feat/source", "feat/forked")
        assert result2.returncode != 0
        assert "already exists" in result2.stderr

    def test_fork_fails_if_source_missing(self, env):
        """Fork fails with helpful error for nonexistent source."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])

        result = env.group_fork("g", "feat/nonexistent", "feat/forked")
        assert result.returncode != 0
        assert "not found" in result.stderr

    def test_fork_fails_if_branch_exists_in_repo(self, env):
        """Fork fails if the target branch already exists in any repo."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        # Manually create the target branch
        subprocess.run(
            ["git", "branch", "feat/taken"],
            cwd=repo, check=True, capture_output=True,
        )

        result = env.group_fork("g", "feat/source", "feat/taken")
        assert result.returncode != 0
        assert "already exists" in result.stderr


class TestGroupForkMultiRepo:
    """Multi-repo scenarios."""

    def test_fork_multi_repo_mixed_dirty(self, env):
        """Fork across repos where one is dirty and another is clean."""
        repo1 = env.create_repo("repo1")
        repo2 = env.create_repo("repo2")
        env.configure_group("g", [repo1, repo2])
        env.group_add("g", "feat/source")

        # Make repo1 dirty, leave repo2 clean
        wt1 = env.worktree_path(repo1, "feat/source")
        (wt1 / "dirty.txt").write_text("only in repo1")

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0, f"fork failed: {result.stderr}"

        fork_wt1 = env.worktree_path(repo1, "feat/forked")
        fork_wt2 = env.worktree_path(repo2, "feat/forked")

        # repo1 fork has the dirty file
        assert (fork_wt1 / "dirty.txt").read_text() == "only in repo1"

        # repo2 fork is clean
        status = subprocess.run(
            ["git", "status", "--porcelain"], cwd=fork_wt2,
            check=True, capture_output=True, text=True,
        )
        assert status.stdout.strip() == ""

    def test_fork_multi_repo_diverged_heads(self, env):
        """Fork preserves per-repo HEAD even when repos have diverged."""
        repo1 = env.create_repo("repo1")
        repo2 = env.create_repo("repo2")
        env.configure_group("g", [repo1, repo2])
        env.group_add("g", "feat/source")

        # Commit in repo1 source only
        wt1 = env.worktree_path(repo1, "feat/source")
        (wt1 / "r1.txt").write_text("repo1 commit")
        subprocess.run(["git", "add", "."], cwd=wt1, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "r1"], cwd=wt1, check=True, capture_output=True)
        r1_head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=wt1,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

        wt2 = env.worktree_path(repo2, "feat/source")
        r2_head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=wt2,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

        assert r1_head != r2_head  # repos diverged

        result = env.group_fork("g", "feat/source", "feat/forked")
        assert result.returncode == 0

        # Each fork has its repo's HEAD
        fork_wt1 = env.worktree_path(repo1, "feat/forked")
        fork_wt2 = env.worktree_path(repo2, "feat/forked")

        f1_head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=fork_wt1,
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        f2_head = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=fork_wt2,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

        assert f1_head == r1_head
        assert f2_head == r2_head


class TestGroupForkLifecycle:
    """Lifecycle: fork interacts correctly with other group commands."""

    def test_fork_then_remove(self, env):
        """A forked workspace can be cleanly removed."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")
        env.group_fork("g", "feat/source", "feat/forked")

        ws = env.workspace_dir("g", "feat/forked")
        wt = env.worktree_path(repo, "feat/forked")
        assert ws.exists()
        assert wt.exists()

        result = env.run_workmux("group", "remove", "g", "feat/forked", "-f")
        assert result.returncode == 0

        assert not ws.exists()
        assert not wt.exists()

    def test_fork_then_fork(self, env):
        """Can chain forks: A -> B -> C."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/a")

        # Commit on A
        wt_a = env.worktree_path(repo, "feat/a")
        (wt_a / "a.txt").write_text("from a")
        subprocess.run(["git", "add", "."], cwd=wt_a, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "a"], cwd=wt_a, check=True, capture_output=True)

        # Fork A -> B
        result_b = env.group_fork("g", "feat/a", "feat/b")
        assert result_b.returncode == 0

        # Commit on B
        wt_b = env.worktree_path(repo, "feat/b")
        (wt_b / "b.txt").write_text("from b")
        subprocess.run(["git", "add", "."], cwd=wt_b, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "b"], cwd=wt_b, check=True, capture_output=True)

        # Fork B -> C
        result_c = env.group_fork("g", "feat/b", "feat/c")
        assert result_c.returncode == 0

        wt_c = env.worktree_path(repo, "feat/c")
        assert (wt_c / "a.txt").read_text() == "from a"
        assert (wt_c / "b.txt").read_text() == "from b"

    def test_remove_fork_leaves_source(self, env):
        """Removing a fork doesn't affect the source workspace."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")
        env.group_fork("g", "feat/source", "feat/forked")

        env.run_workmux("group", "remove", "g", "feat/forked", "-f")

        # Source still works
        source_ws = env.workspace_dir("g", "feat/source")
        source_wt = env.worktree_path(repo, "feat/source")
        assert source_ws.exists()
        assert source_wt.exists()

        result = env.run_workmux("group", "status", "g", "feat/source", "--json")
        assert result.returncode == 0


class TestGroupForkMetadata:
    """Metadata propagation: workmux-base, ship strategy, vscode workspace."""

    def test_fork_generates_vscode_workspace(self, env):
        """Fork creates a .code-workspace file."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")

        env.group_fork("g", "feat/source", "feat/forked")

        ws = env.workspace_dir("g", "feat/forked")
        workspace_files = list(ws.glob("*.code-workspace"))
        assert len(workspace_files) == 1

    def test_fork_appears_in_group_list(self, env):
        """Forked workspace shows up in group list."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")
        env.group_fork("g", "feat/source", "feat/forked")

        result = env.run_workmux("group", "list", "--json")
        assert result.returncode == 0
        data = json.loads(result.stdout)
        branches = [g["branch"] for g in data]
        assert "feat/source" in branches
        assert "feat/forked" in branches

    def test_fork_status_shows_clean(self, env):
        """Forked workspace (from clean source) shows clean status."""
        repo = env.create_repo("repo")
        env.configure_group("g", [repo])
        env.group_add("g", "feat/source")
        env.group_fork("g", "feat/source", "feat/forked")

        result = env.run_workmux("group", "status", "g", "feat/forked", "--json")
        assert result.returncode == 0
        data = json.loads(result.stdout)
        assert data["branch"] == "feat/forked"
        for repo_status in data["repos"]:
            assert repo_status["worktree_exists"] is True
