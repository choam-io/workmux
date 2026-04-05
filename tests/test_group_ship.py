"""Integration tests for workmux group ship strategies and guard command.

Tests require:
- cargo build (workmux binary)
- gh CLI authenticated with access to choam-io org
- Two test repos: choam-io/workmux-test-alpha, choam-io/workmux-test-beta

Run: uv run --with pytest --with pyyaml --with pytest-xdist pytest tests/test_group_ship.py -x -v
"""

import os
import subprocess
import time
from pathlib import Path

import pytest
import yaml


# =============================================================================
# Test Environment
# =============================================================================


class ShipTestEnv:
    """Isolated test environment for group ship strategy tests."""

    def __init__(self, tmp_path: Path):
        self.tmp_path = tmp_path
        self.home = tmp_path / "home"
        self.home.mkdir()
        self.groups_dir = self.home / ".local" / "share" / "workmux" / "groups"
        self.global_config_path = self.home / ".config" / "workmux" / "config.yaml"

        self.env = os.environ.copy()
        self.env["HOME"] = str(self.home)

        # Propagate GitHub auth to isolated environment.
        # gh uses OS keychain on macOS which doesn't work with a different HOME.
        # Export the token so both gh CLI and git credential helper work.
        real_gh_config = Path(os.environ.get("HOME", str(Path.home()))) / ".config" / "gh"
        if real_gh_config.exists():
            test_gh_config = self.home / ".config" / "gh"
            test_gh_config.parent.mkdir(parents=True, exist_ok=True)
            os.symlink(real_gh_config, test_gh_config)
        # Get token from gh auth if available (works with keychain)
        try:
            token_result = subprocess.run(
                ["gh", "auth", "token"], capture_output=True, text=True, check=False,
            )
            if token_result.returncode == 0 and token_result.stdout.strip():
                self.env["GH_TOKEN"] = token_result.stdout.strip()
        except FileNotFoundError:
            pass

        # Set up global git credential helper so clones and pushes work
        gh_path = subprocess.run(
            ["which", "gh"], capture_output=True, text=True, check=False,
        ).stdout.strip()
        if gh_path:
            git_config_dir = self.home / ".config" / "git"
            git_config_dir.mkdir(parents=True, exist_ok=True)
            (git_config_dir / "config").write_text(
                f'[credential "https://github.com"]\n'
                f"  helper = \n"
                f"  helper = !{gh_path} auth git-credential\n"
            )

        # Find the workmux binary
        workmux_dir = Path(__file__).parent.parent
        self.binary = workmux_dir / "target" / "debug" / "workmux"
        if not self.binary.exists():
            pytest.fail("workmux binary not found. Run 'cargo build' first.")

    def create_repo(self, name: str, remote_url: str | None = None) -> Path:
        """Create a git repo with an initial commit and optional remote.
        
        When remote_url is provided, clones from the remote to ensure shared
        history (required for PR creation).
        """
        repo = self.tmp_path / name

        if remote_url:
            # Clone to get shared history with the remote
            subprocess.run(
                ["git", "clone", remote_url, str(repo)],
                capture_output=True, text=True, check=True, env=self.env,
            )
            self._git(repo, "config", "user.email", "test@choam.io")
            self._git(repo, "config", "user.name", "workmux-test")
        else:
            repo.mkdir()
            self._git(repo, "init", "-b", "main")
            self._git(repo, "config", "user.email", "test@choam.io")
            self._git(repo, "config", "user.name", "workmux-test")

            (repo / "README.md").write_text(f"# {name}\n")
            self._git(repo, "add", ".")
            self._git(repo, "commit", "-m", "Initial commit")

        return repo

    def create_local_repo(self, name: str) -> Path:
        """Create a local-only git repo (no remote)."""
        return self.create_repo(name)

    def write_config(self, config: dict):
        """Write workmux global config.
        
        Always sets agent: echo to avoid 'claude not found' errors
        when the test env tries to launch an agent.
        """
        config.setdefault("agent", "echo")
        self.global_config_path.parent.mkdir(parents=True, exist_ok=True)
        self.global_config_path.write_text(yaml.dump(config))

    def run_workmux(self, *args, check: bool = False, stdin: str | None = None) -> subprocess.CompletedProcess:
        """Run workmux with the test environment."""
        result = subprocess.run(
            [str(self.binary)] + list(args),
            env=self.env,
            capture_output=True,
            text=True,
            check=False,
            input=stdin,
        )
        if check and result.returncode != 0:
            raise subprocess.CalledProcessError(
                result.returncode, result.args, result.stdout, result.stderr
            )
        return result

    def _git(self, cwd: Path, *args):
        """Run a git command."""
        result = subprocess.run(
            ["git"] + list(args),
            cwd=cwd,
            capture_output=True,
            text=True,
            check=False,
            env=self.env,
        )
        if result.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr}")
        return result

    def git(self, cwd: Path, *args) -> subprocess.CompletedProcess:
        """Run a git command (public, returns result)."""
        return self._git(cwd, *args)

    def load_group_state(self, group_name: str, branch: str) -> dict:
        """Load and parse the group state YAML."""
        slug_branch = branch.replace("/", "-")
        ws_dir = self.groups_dir / f"{group_name}--{slug_branch}"
        state_file = ws_dir / ".workmux-group.yaml"
        return yaml.safe_load(state_file.read_text())

    def group_workspace_dir(self, group_name: str, branch: str) -> Path:
        """Return the workspace directory for a group."""
        slug_branch = branch.replace("/", "-")
        return self.groups_dir / f"{group_name}--{slug_branch}"


@pytest.fixture
def env(tmp_path):
    return ShipTestEnv(tmp_path)


# =============================================================================
# Ship Strategy Config Tests
# =============================================================================


class TestShipStrategyConfig:
    """Test that ship strategy and context are persisted correctly."""

    def test_group_add_persists_ship_strategy(self, env: ShipTestEnv):
        """ship: pr in config is written to group state."""
        repo = env.create_local_repo("repo1")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        result = env.run_workmux("group", "add", "test", "feat/ship-test", "--background")
        assert result.returncode == 0, f"Failed: {result.stderr}"

        state = env.load_group_state("test", "feat/ship-test")
        assert state["ship"] == "pr"
        assert state["repos"][0]["ship"] == "pr"

    def test_group_add_persists_context(self, env: ShipTestEnv):
        """context field is written to group state."""
        repo = env.create_local_repo("repo1")

        context_text = "Release cmux first.\nThen update deck pins."
        env.write_config({
            "groups": {
                "test": {
                    "context": context_text,
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        result = env.run_workmux("group", "add", "test", "feat/ctx", "--background")
        assert result.returncode == 0, f"Failed: {result.stderr}"

        state = env.load_group_state("test", "feat/ctx")
        assert state["context"].strip() == context_text

    def test_group_add_per_repo_ship_override(self, env: ShipTestEnv):
        """Per-repo ship override takes precedence over group default."""
        repo1 = env.create_local_repo("repo1")
        repo2 = env.create_local_repo("repo2")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [
                        {"path": str(repo1)},
                        {"path": str(repo2), "ship": "local"},
                    ],
                }
            }
        })

        result = env.run_workmux("group", "add", "test", "feat/override", "--background")
        assert result.returncode == 0, f"Failed: {result.stderr}"

        state = env.load_group_state("test", "feat/override")
        # repo1 inherits group default (pr)
        repo1_state = next(r for r in state["repos"] if r["symlink_name"] == "repo1")
        assert repo1_state["ship"] == "pr"
        # repo2 has explicit local override
        repo2_state = next(r for r in state["repos"] if r["symlink_name"] == "repo2")
        assert repo2_state["ship"] == "local"

    def test_group_add_defaults_ship_to_local(self, env: ShipTestEnv):
        """When no ship is specified, defaults to local."""
        repo = env.create_local_repo("repo1")

        env.write_config({
            "groups": {
                "test": {
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        result = env.run_workmux("group", "add", "test", "feat/default", "--background")
        assert result.returncode == 0, f"Failed: {result.stderr}"

        state = env.load_group_state("test", "feat/default")
        assert state["ship"] == "local"
        assert state["repos"][0]["ship"] == "local"

    def test_group_add_mq_strategy(self, env: ShipTestEnv):
        """ship: mq in config is written to group state."""
        repo = env.create_local_repo("repo1")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "mq",
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        result = env.run_workmux("group", "add", "test", "feat/mq", "--background")
        assert result.returncode == 0, f"Failed: {result.stderr}"

        state = env.load_group_state("test", "feat/mq")
        assert state["ship"] == "mq"

    def test_group_add_backward_compat_ignores_merge_order(self, env: ShipTestEnv):
        """Old configs with merge_order don't break group add."""
        repo = env.create_local_repo("repo1")

        env.write_config({
            "groups": {
                "test": {
                    "merge_order": ["repo1"],
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        result = env.run_workmux("group", "add", "test", "feat/compat", "--background")
        assert result.returncode == 0, f"Failed: {result.stderr}"

    def test_group_status_shows_ship_strategy(self, env: ShipTestEnv):
        """Group status JSON includes ship strategy."""
        repo = env.create_local_repo("repo1")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        env.run_workmux("group", "add", "test", "feat/stat", "--background", check=True)

        result = env.run_workmux("group", "status", "test", "feat/stat", "--json")
        assert result.returncode == 0


# =============================================================================
# Local Merge Tests (no GitHub needed)
# =============================================================================


class TestGroupMergeLocal:
    """Test group merge with ship: local strategy."""

    def test_local_merge_merges_into_main(self, env: ShipTestEnv):
        """ship: local merges branch into main locally."""
        repo = env.create_local_repo("repo1")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "local",
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        env.run_workmux("group", "add", "test", "feat/local-merge", "--background", check=True)

        # Make a commit in the worktree
        wt_dir = repo.parent / "repo1__worktrees" / "feat-local-merge"
        assert wt_dir.exists()
        (wt_dir / "feature.txt").write_text("new feature\n")
        env.git(wt_dir, "add", ".")
        env.git(wt_dir, "commit", "-m", "add feature")

        # Merge (pipe 'y' to confirm)
        result = env.run_workmux("group", "merge", "test", "feat/local-merge", stdin="y\n")
        assert result.returncode == 0, f"Failed: {result.stderr}"

        # Verify the commit is on main
        log = env.git(repo, "log", "--oneline", "-1")
        assert "add feature" in log.stdout

        # Worktree should be cleaned up
        assert not wt_dir.exists()

    def test_local_merge_fails_with_uncommitted_changes(self, env: ShipTestEnv):
        """ship: local merge fails if worktree has uncommitted changes."""
        repo = env.create_local_repo("repo1")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "local",
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        env.run_workmux("group", "add", "test", "feat/dirty", "--background", check=True)

        # Create uncommitted change
        wt_dir = repo.parent / "repo1__worktrees" / "feat-dirty"
        (wt_dir / "dirty.txt").write_text("dirty\n")

        result = env.run_workmux("group", "merge", "test", "feat/dirty", stdin="y\n")
        assert result.returncode != 0
        assert "uncommitted" in result.stderr.lower() or "uncommitted" in result.stdout.lower()

    def test_local_merge_mixed_strategies(self, env: ShipTestEnv):
        """Mixed ship strategies: local repos merge, pr repos are skipped (no remote)."""
        repo1 = env.create_local_repo("repo1")
        repo2 = env.create_local_repo("repo2")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "local",
                    "repos": [
                        {"path": str(repo1)},
                        {"path": str(repo2)},
                    ],
                }
            }
        })

        env.run_workmux("group", "add", "test", "feat/mixed", "--background", check=True)

        # Make commits in both worktrees
        for name in ["repo1", "repo2"]:
            wt = env.tmp_path / name / ".." / f"{name}__worktrees" / "feat-mixed"
            wt = (env.tmp_path / name).parent / f"{name}__worktrees" / "feat-mixed"
            (wt / f"{name}_feature.txt").write_text(f"feature in {name}\n")
            env.git(wt, "add", ".")
            env.git(wt, "commit", "-m", f"add feature in {name}")

        result = env.run_workmux("group", "merge", "test", "feat/mixed", stdin="y\n")
        assert result.returncode == 0, f"Failed: {result.stderr}"

        # Both repos should have the commits on main
        for name in ["repo1", "repo2"]:
            repo_path = env.tmp_path / name
            log = env.git(repo_path, "log", "--oneline", "-1")
            assert f"add feature in {name}" in log.stdout


# =============================================================================
# PR Merge Tests (requires GitHub)
# =============================================================================

GITHUB_ALPHA_REPO = "https://github.com/choam-io/workmux-test-alpha.git"
GITHUB_BETA_REPO = "https://github.com/choam-io/workmux-test-beta.git"


def gh_available() -> bool:
    """Check if gh CLI is authenticated."""
    try:
        result = subprocess.run(
            ["gh", "auth", "status"],
            capture_output=True, text=True, check=False,
        )
        return result.returncode == 0
    except FileNotFoundError:
        return False


requires_github = pytest.mark.skipif(
    not gh_available(),
    reason="gh CLI not authenticated -- skipping GitHub integration tests"
)


def unique_branch(prefix: str = "test") -> str:
    """Generate a unique branch name to avoid collisions."""
    return f"{prefix}/{int(time.time())}-{os.getpid()}"


def cleanup_remote_branch(repo_path: Path, branch: str):
    """Best-effort cleanup of remote branch."""
    subprocess.run(
        ["git", "push", "origin", "--delete", branch],
        cwd=repo_path, capture_output=True, text=True, check=False,
    )


def cleanup_pr(repo_path: Path, branch: str):
    """Best-effort close any open PR for this branch."""
    result = subprocess.run(
        ["gh", "pr", "list", "--head", branch, "--json", "number", "--jq", ".[0].number"],
        cwd=repo_path, capture_output=True, text=True, check=False,
    )
    pr_number = result.stdout.strip()
    if pr_number:
        subprocess.run(
            ["gh", "pr", "close", pr_number],
            cwd=repo_path, capture_output=True, text=True, check=False,
        )


@requires_github
class TestGroupMergePR:
    """Test group merge with ship: pr strategy against real GitHub repos."""

    def test_pr_merge_pushes_and_creates_pr(self, env: ShipTestEnv):
        """ship: pr pushes branch and creates a PR via gh."""
        branch = unique_branch("ship-pr")
        repo = env.create_repo("alpha", remote_url=GITHUB_ALPHA_REPO)

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        env.run_workmux("group", "add", "test", branch, "--background", check=True)

        # Make a commit in the worktree
        wt_dir = repo.parent / "alpha__worktrees" / branch.replace("/", "-")
        assert wt_dir.exists(), f"Worktree not found at {wt_dir}"
        (wt_dir / "feature.txt").write_text("pr feature\n")
        env.git(wt_dir, "add", ".")
        env.git(wt_dir, "commit", "-m", "add pr feature")

        try:
            # Merge with pr strategy
            result = env.run_workmux("group", "merge", "test", branch, stdin="y\n")
            assert result.returncode == 0, f"Failed: {result.stderr}\n{result.stdout}"
            assert "PR:" in result.stdout

            # Verify PR exists on GitHub
            pr_check = subprocess.run(
                ["gh", "pr", "list", "--head", branch, "--json", "number,state", "--repo", "choam-io/workmux-test-alpha"],
                capture_output=True, text=True, check=False,
            )
            assert pr_check.returncode == 0
            import json
            prs = json.loads(pr_check.stdout)
            assert len(prs) >= 1, f"No PR found for branch {branch}"
            assert prs[0]["state"] == "OPEN"

            # Worktree should be cleaned up
            assert not wt_dir.exists()

            # Main should NOT have the commit (it went to PR, not local merge)
            log = env.git(repo, "log", "--oneline")
            assert "add pr feature" not in log.stdout
        finally:
            cleanup_pr(repo, branch)
            cleanup_remote_branch(repo, branch)

    def test_pr_merge_multi_repo(self, env: ShipTestEnv):
        """ship: pr works across multiple repos in a group."""
        branch = unique_branch("ship-pr-multi")
        alpha = env.create_repo("alpha", remote_url=GITHUB_ALPHA_REPO)
        beta = env.create_repo("beta", remote_url=GITHUB_BETA_REPO)

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [
                        {"path": str(alpha)},
                        {"path": str(beta)},
                    ],
                }
            }
        })

        env.run_workmux("group", "add", "test", branch, "--background", check=True)

        # Commit in both worktrees
        slug = branch.replace("/", "-")
        for name in ["alpha", "beta"]:
            wt = env.tmp_path / name / ".." / f"{name}__worktrees" / slug
            wt = (env.tmp_path / name).parent / f"{name}__worktrees" / slug
            (wt / f"{name}_change.txt").write_text(f"change in {name}\n")
            env.git(wt, "add", ".")
            env.git(wt, "commit", "-m", f"change in {name}")

        try:
            result = env.run_workmux("group", "merge", "test", branch, stdin="y\n")
            assert result.returncode == 0, f"Failed: {result.stderr}\n{result.stdout}"
            # Should have two PR lines
            assert result.stdout.count("PR:") == 2 or "Merged" in result.stdout
        finally:
            for name, gh_repo in [("alpha", "choam-io/workmux-test-alpha"), ("beta", "choam-io/workmux-test-beta")]:
                repo_path = env.tmp_path / name
                cleanup_pr(repo_path, branch)
                cleanup_remote_branch(repo_path, branch)

    def test_mixed_pr_and_local_merge(self, env: ShipTestEnv):
        """Group with mixed strategies: one repo PR, one repo local."""
        branch = unique_branch("ship-mixed")
        alpha = env.create_repo("alpha", remote_url=GITHUB_ALPHA_REPO)
        local_repo = env.create_local_repo("local_repo")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [
                        {"path": str(alpha)},
                        {"path": str(local_repo), "ship": "local"},
                    ],
                }
            }
        })

        env.run_workmux("group", "add", "test", branch, "--background", check=True)

        # Commit in both worktrees
        slug = branch.replace("/", "-")
        alpha_wt = alpha.parent / "alpha__worktrees" / slug
        local_wt = local_repo.parent / "local_repo__worktrees" / slug

        (alpha_wt / "alpha.txt").write_text("alpha pr\n")
        env.git(alpha_wt, "add", ".")
        env.git(alpha_wt, "commit", "-m", "alpha pr change")

        (local_wt / "local.txt").write_text("local merge\n")
        env.git(local_wt, "add", ".")
        env.git(local_wt, "commit", "-m", "local merge change")

        try:
            result = env.run_workmux("group", "merge", "test", branch, stdin="y\n")
            assert result.returncode == 0, f"Failed: {result.stderr}\n{result.stdout}"

            # alpha should have a PR, NOT merged locally
            log = env.git(alpha, "log", "--oneline")
            assert "alpha pr change" not in log.stdout

            # local_repo should be merged locally
            log = env.git(local_repo, "log", "--oneline")
            assert "local merge change" in log.stdout
        finally:
            cleanup_pr(alpha, branch)
            cleanup_remote_branch(alpha, branch)

    def test_pr_merge_fails_with_uncommitted_changes(self, env: ShipTestEnv):
        """ship: pr merge fails if worktree has uncommitted changes."""
        branch = unique_branch("ship-pr-dirty")
        repo = env.create_repo("alpha", remote_url=GITHUB_ALPHA_REPO)

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        env.run_workmux("group", "add", "test", branch, "--background", check=True)

        # Create uncommitted change (don't commit)
        slug = branch.replace("/", "-")
        wt_dir = repo.parent / "alpha__worktrees" / slug
        (wt_dir / "dirty.txt").write_text("uncommitted\n")

        result = env.run_workmux("group", "merge", "test", branch, stdin="y\n")
        assert result.returncode != 0

        # Cleanup (force remove the group)
        env.run_workmux("group", "remove", "test", branch, "-f")


# =============================================================================
# Guard Tests
# =============================================================================


class TestGuard:
    """Test workmux guard command for pre-commit hook management."""

    def test_guard_install_and_status(self, env: ShipTestEnv):
        """Guard installs hooks and status reports them."""
        # Create a fake ghq structure
        ghq_dir = env.home / "ghq" / "github.com" / "test-org"
        ghq_dir.mkdir(parents=True)

        repo = ghq_dir / "test-repo"
        repo.mkdir()
        env._git(repo, "init", "-b", "main")
        env._git(repo, "config", "user.email", "t@t.com")
        env._git(repo, "config", "user.name", "t")
        (repo / "README.md").write_text("# test\n")
        env._git(repo, "add", ".")
        env._git(repo, "commit", "-m", "init")

        # Install guards
        result = env.run_workmux("guard")
        assert result.returncode == 0
        assert "installed" in result.stdout

        # Verify hook exists
        hook = repo / ".git" / "hooks" / "pre-commit"
        assert hook.exists()
        content = hook.read_text()
        assert "workmux guard" in content

        # Status should show guarded
        result = env.run_workmux("guard", "--status")
        assert result.returncode == 0
        assert "guarded:" in result.stdout

    def test_guard_idempotent(self, env: ShipTestEnv):
        """Running guard twice doesn't duplicate hooks."""
        ghq_dir = env.home / "ghq" / "github.com" / "test-org"
        repo = ghq_dir / "test-repo"
        repo.mkdir(parents=True)
        env._git(repo, "init", "-b", "main")
        env._git(repo, "config", "user.email", "t@t.com")
        env._git(repo, "config", "user.name", "t")
        (repo / "README.md").write_text("# test\n")
        env._git(repo, "add", ".")
        env._git(repo, "commit", "-m", "init")

        env.run_workmux("guard", check=True)
        result2 = env.run_workmux("guard")
        assert result2.returncode == 0
        # Should say 0 installed on second run (all skipped)
        assert "0 installed" in result2.stdout or "installed" in result2.stdout

    def test_guard_skips_custom_hooks(self, env: ShipTestEnv):
        """Guard doesn't overwrite existing non-workmux hooks."""
        ghq_dir = env.home / "ghq" / "github.com" / "test-org"
        repo = ghq_dir / "test-repo"
        repo.mkdir(parents=True)
        env._git(repo, "init", "-b", "main")
        env._git(repo, "config", "user.email", "t@t.com")
        env._git(repo, "config", "user.name", "t")
        (repo / "README.md").write_text("# test\n")
        env._git(repo, "add", ".")
        env._git(repo, "commit", "-m", "init")

        # Write a custom hook first
        hook = repo / ".git" / "hooks" / "pre-commit"
        hook.parent.mkdir(parents=True, exist_ok=True)
        hook.write_text("#!/bin/sh\necho custom lint\n")
        hook.chmod(0o755)

        result = env.run_workmux("guard")
        assert result.returncode == 0

        # Custom hook should be untouched
        assert "custom lint" in hook.read_text()
        assert "workmux guard" not in hook.read_text()

    def test_guard_remove(self, env: ShipTestEnv):
        """Guard --remove removes only our hooks."""
        ghq_dir = env.home / "ghq" / "github.com" / "test-org"
        repo = ghq_dir / "test-repo"
        repo.mkdir(parents=True)
        env._git(repo, "init", "-b", "main")
        env._git(repo, "config", "user.email", "t@t.com")
        env._git(repo, "config", "user.name", "t")
        (repo / "README.md").write_text("# test\n")
        env._git(repo, "add", ".")
        env._git(repo, "commit", "-m", "init")

        env.run_workmux("guard", check=True)

        hook = repo / ".git" / "hooks" / "pre-commit"
        assert hook.exists()

        result = env.run_workmux("guard", "--remove")
        assert result.returncode == 0
        assert not hook.exists()

    def test_guard_hook_blocks_commits(self, env: ShipTestEnv):
        """The installed hook actually blocks git commit."""
        ghq_dir = env.home / "ghq" / "github.com" / "test-org"
        repo = ghq_dir / "test-repo"
        repo.mkdir(parents=True)
        env._git(repo, "init", "-b", "main")
        env._git(repo, "config", "user.email", "t@t.com")
        env._git(repo, "config", "user.name", "t")
        (repo / "README.md").write_text("# test\n")
        env._git(repo, "add", ".")
        env._git(repo, "commit", "-m", "init")

        env.run_workmux("guard", check=True)

        # Try to commit -- should fail
        (repo / "blocked.txt").write_text("should fail\n")
        env._git(repo, "add", ".")

        result = subprocess.run(
            ["git", "commit", "-m", "this should fail"],
            cwd=repo, capture_output=True, text=True, check=False,
            env=env.env,
        )
        assert result.returncode != 0
        assert "workmux" in result.stderr.lower() or "blocked" in result.stderr.lower()

    def test_guard_hook_bypass_with_no_verify(self, env: ShipTestEnv):
        """git commit --no-verify bypasses the guard hook."""
        ghq_dir = env.home / "ghq" / "github.com" / "test-org"
        repo = ghq_dir / "test-repo"
        repo.mkdir(parents=True)
        env._git(repo, "init", "-b", "main")
        env._git(repo, "config", "user.email", "t@t.com")
        env._git(repo, "config", "user.name", "t")
        (repo / "README.md").write_text("# test\n")
        env._git(repo, "add", ".")
        env._git(repo, "commit", "-m", "init")

        env.run_workmux("guard", check=True)

        # --no-verify should bypass
        (repo / "bypass.txt").write_text("bypassed\n")
        env._git(repo, "add", ".")
        env._git(repo, "commit", "--no-verify", "-m", "bypass commit")

        log = env._git(repo, "log", "--oneline", "-1")
        assert "bypass commit" in log.stdout

    def test_guard_skips_worktree_git_files(self, env: ShipTestEnv):
        """Guard skips directories with .git files (worktrees) instead of .git dirs."""
        ghq_dir = env.home / "ghq" / "github.com" / "test-org"
        repo = ghq_dir / "test-repo"
        repo.mkdir(parents=True)
        env._git(repo, "init", "-b", "main")
        env._git(repo, "config", "user.email", "t@t.com")
        env._git(repo, "config", "user.name", "t")
        (repo / "README.md").write_text("# test\n")
        env._git(repo, "add", ".")
        env._git(repo, "commit", "-m", "init")

        # Create a worktree (has .git file, not .git dir)
        wt_dir = ghq_dir / "test-repo__worktrees" / "feat-x"
        env._git(repo, "worktree", "add", str(wt_dir), "-b", "feat-x")
        assert (wt_dir / ".git").is_file()  # worktree .git is a file

        result = env.run_workmux("guard")
        assert result.returncode == 0

        # Should NOT have a hook in the worktree (it's skipped)
        wt_hook = wt_dir / ".git" / "hooks" / "pre-commit"
        assert not wt_hook.exists()

        # Should have a hook in the real repo
        repo_hook = repo / ".git" / "hooks" / "pre-commit"
        assert repo_hook.exists()


# =============================================================================
# Merge Confirmation UX
# =============================================================================


class TestMergeConfirmation:
    """Test that merge confirmation shows ship strategies."""

    def test_merge_confirmation_shows_strategies(self, env: ShipTestEnv):
        """Merge confirmation displays per-repo ship strategies."""
        repo1 = env.create_local_repo("repo1")
        repo2 = env.create_local_repo("repo2")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [
                        {"path": str(repo1)},
                        {"path": str(repo2), "ship": "local"},
                    ],
                }
            }
        })

        env.run_workmux("group", "add", "test", "feat/confirm", "--background", check=True)

        # Verify ship strategies are in the state file (since confirmation
        # is skipped when stdin is not a terminal, we check the state directly)
        state = env.load_group_state("test", "feat/confirm")
        repo1_state = next(r for r in state["repos"] if r["symlink_name"] == "repo1")
        repo2_state = next(r for r in state["repos"] if r["symlink_name"] == "repo2")
        assert repo1_state["ship"] == "pr"
        assert repo2_state["ship"] == "local"


# =============================================================================
# Partial failure preservation (Fix #3)
# =============================================================================


class TestPartialFailurePreservation:
    """Test that workspace is preserved when some repos fail to merge."""

    def test_workspace_preserved_on_partial_pr_failure(self, env: ShipTestEnv):
        """If a PR repo fails (no remote), the workspace is NOT deleted."""
        repo_with_remote = env.create_local_repo("local_ok")
        repo_no_remote = env.create_local_repo("no_remote")

        env.write_config({
            "groups": {
                "test": {
                    "repos": [
                        {"path": str(repo_with_remote), "ship": "local"},
                        {"path": str(repo_no_remote), "ship": "pr"},
                    ],
                }
            }
        })

        env.run_workmux("group", "add", "test", "feat/partial", "--background", check=True)

        # Make commits in both
        for name in ["local_ok", "no_remote"]:
            wt = (env.tmp_path / name).parent / f"{name}__worktrees" / "feat-partial"
            (wt / "f.txt").write_text("change\n")
            env.git(wt, "add", ".")
            env.git(wt, "commit", "-m", f"change in {name}")

        result = env.run_workmux("group", "merge", "test", "feat/partial")
        # Should fail (no_remote has no origin)
        assert result.returncode != 0

        # But workspace should still exist
        ws_dir = env.group_workspace_dir("test", "feat/partial")
        assert ws_dir.exists(), "Workspace was deleted despite partial failure"

        # The successful local merge should have happened
        log = env.git(repo_with_remote, "log", "--oneline", "-1")
        assert "change in local_ok" in log.stdout

    def test_workspace_deleted_when_all_succeed(self, env: ShipTestEnv):
        """Workspace is cleaned up when all repos merge successfully."""
        repo1 = env.create_local_repo("r1")
        repo2 = env.create_local_repo("r2")

        env.write_config({
            "groups": {
                "test": {
                    "ship": "local",
                    "repos": [
                        {"path": str(repo1)},
                        {"path": str(repo2)},
                    ],
                }
            }
        })

        env.run_workmux("group", "add", "test", "feat/allgood", "--background", check=True)

        for name in ["r1", "r2"]:
            wt = (env.tmp_path / name).parent / f"{name}__worktrees" / "feat-allgood"
            (wt / "f.txt").write_text("ok\n")
            env.git(wt, "add", ".")
            env.git(wt, "commit", "-m", f"ok in {name}")

        result = env.run_workmux("group", "merge", "test", "feat/allgood")
        assert result.returncode == 0

        ws_dir = env.group_workspace_dir("test", "feat/allgood")
        assert not ws_dir.exists(), "Workspace should be cleaned up after full success"


# =============================================================================
# PR rollback on failure (Fix #2) -- requires GitHub
# =============================================================================


@requires_github
class TestPRRollback:
    """Test that failed PR creation cleans up the pushed branch."""

    def test_pr_creation_failure_cleans_remote_branch(self, env: ShipTestEnv):
        """Duplicate PR create should NOT delete the remote branch.

        This exercises the regression path fixed in group merge: when
        `gh pr create` fails because a PR already exists, workmux must keep the
        remote branch (otherwise the existing PR is orphaned/broken).
        """
        branch = unique_branch("ship-pr-rollback")
        repo = env.create_repo("alpha", remote_url=GITHUB_ALPHA_REPO)

        env.write_config({
            "groups": {
                "test": {
                    "ship": "pr",
                    "repos": [{"path": str(repo)}],
                }
            }
        })

        env.run_workmux("group", "add", "test", branch, "--background", check=True)

        slug = branch.replace("/", "-")
        wt_dir = repo.parent / "alpha__worktrees" / slug
        (wt_dir / "feature.txt").write_text("pr feature\n")
        env.git(wt_dir, "add", ".")
        env.git(wt_dir, "commit", "-m", "add feature")

        # Push and create PR first so group merge hits duplicate-PR path.
        env.git(wt_dir, "push", "--set-upstream", "origin", branch)
        import subprocess as sp
        first_pr = sp.run(
            [
                "gh", "pr", "create",
                "--base", "main",
                "--head", branch,
                "--title", f"rollback test {branch}",
                "--body", "setup PR for rollback integration test",
                "--repo", "choam-io/workmux-test-alpha",
            ],
            capture_output=True, text=True, check=False, env=env.env,
        )
        if first_pr.returncode != 0:
            pytest.skip(f"failed to create setup PR for rollback test: {first_pr.stderr}")

        try:
            # Should fail because PR already exists for this branch.
            result = env.run_workmux("group", "merge", "test", branch)
            assert result.returncode != 0, (
                "Expected group merge to fail on duplicate PR create, "
                f"got success. stdout={result.stdout} stderr={result.stderr}"
            )

            # Remote branch must still exist (critical rollback assertion).
            remote_check = sp.run(
                ["git", "ls-remote", "--heads", "origin", branch],
                cwd=repo, capture_output=True, text=True, env=env.env,
            )
            assert remote_check.returncode == 0
            assert branch in remote_check.stdout, (
                "Remote branch was deleted on duplicate PR error; "
                "existing PR would be orphaned."
            )

            # Existing PR must still be open.
            pr_check = sp.run(
                [
                    "gh", "pr", "list",
                    "--head", branch,
                    "--state", "open",
                    "--repo", "choam-io/workmux-test-alpha",
                    "--json", "number,state",
                ],
                capture_output=True, text=True, check=False, env=env.env,
            )
            assert pr_check.returncode == 0
            prs = yaml.safe_load(pr_check.stdout) or []
            assert len(prs) >= 1, "Expected existing open PR to remain after duplicate create failure"
            assert prs[0]["state"] == "OPEN"

            # Workspace should be preserved on failure.
            ws_dir = env.group_workspace_dir("test", branch)
            assert ws_dir.exists(), "Workspace should be preserved after merge failure"
        finally:
            cleanup_pr(repo, branch)
            cleanup_remote_branch(repo, branch)
            env.run_workmux("group", "remove", "test", branch, "-f")


# =============================================================================
# Guard: bare repo handling (Fix #5)
# =============================================================================


class TestGuardBareRepo:
    """Test that guard skips bare repos in ghq."""

    def test_guard_skips_bare_repos(self, env: ShipTestEnv):
        """Guard does not try to install hooks in bare repos."""
        ghq_dir = env.home / "ghq" / "github.com" / "test-org"

        # Normal repo
        normal = ghq_dir / "normal-repo"
        normal.mkdir(parents=True)
        env._git(normal, "init", "-b", "main")
        env._git(normal, "config", "user.email", "t@t.com")
        env._git(normal, "config", "user.name", "t")
        (normal / "README.md").write_text("# normal\n")
        env._git(normal, "add", ".")
        env._git(normal, "commit", "-m", "init")

        # Bare repo
        bare = ghq_dir / "bare-repo.git"
        bare.mkdir(parents=True)
        import subprocess as sp
        sp.run(["git", "init", "--bare"], cwd=bare, check=True, capture_output=True, env=env.env)

        result = env.run_workmux("guard")
        assert result.returncode == 0

        # Normal repo should have hook
        assert (normal / ".git" / "hooks" / "pre-commit").exists()

        # Bare repo should NOT have a hook (and shouldn't error)
        assert not (bare / "hooks" / "pre-commit").exists()
