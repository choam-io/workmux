"""Integration tests for workmux dev environment support.

Tests the full dev-env lifecycle against real GitHub Codespaces.
Uses nodeselector/workmux-dev-env-test as the target repo.

These tests create real codespaces (cheapest tier, short idle/retention).
They clean up after themselves but check for leaks at the end.

Run: uv run --with pytest --with pyyaml --with pytest-xdist pytest tests/test_dev_env.py -x -v
Skip: pytest tests/test_dev_env.py -k "not codespace"
"""

import json
import os
import signal
import subprocess
import time
from pathlib import Path

import pytest
import yaml


TEST_REPO = "nodeselector/workmux-dev-env-test"
TEST_REPO_URL = f"https://github.com/{TEST_REPO}.git"
TEST_MACHINE = "basicLinux32gb"


# =============================================================================
# Helpers
# =============================================================================


def gh_codespace_available() -> bool:
    """Check if gh codespace scope is available."""
    try:
        result = subprocess.run(
            ["gh", "codespace", "list", "--repo", TEST_REPO, "--limit", "1"],
            capture_output=True, text=True, check=False, timeout=15,
        )
        return result.returncode == 0
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False


requires_codespace = pytest.mark.skipif(
    not gh_codespace_available(),
    reason="gh codespace scope not available"
)


def cleanup_codespace(name: str):
    """Best-effort delete a codespace."""
    subprocess.run(
        ["gh", "codespace", "delete", "--codespace", name, "--force"],
        capture_output=True, text=True, check=False, timeout=30,
    )


def list_test_codespaces() -> list[dict]:
    """List all codespaces for the test repo."""
    result = subprocess.run(
        ["gh", "codespace", "list", "--repo", TEST_REPO, "--json", "name,state"],
        capture_output=True, text=True, check=False, timeout=15,
    )
    if result.returncode != 0:
        return []
    return json.loads(result.stdout)


# =============================================================================
# Test Environment
# =============================================================================


class DevEnvTestEnv:
    """Isolated test environment for dev env tests."""

    def __init__(self, tmp_path: Path):
        self.tmp_path = tmp_path
        self.home = tmp_path / "home"
        self.home.mkdir()
        self.groups_dir = self.home / ".local" / "share" / "workmux" / "groups"
        self.global_config_path = self.home / ".config" / "workmux" / "config.yaml"

        self.env = os.environ.copy()
        self.env["HOME"] = str(self.home)

        # Propagate gh auth
        real_gh_config = Path(os.environ.get("HOME", str(Path.home()))) / ".config" / "gh"
        if real_gh_config.exists():
            test_gh_config = self.home / ".config" / "gh"
            test_gh_config.parent.mkdir(parents=True, exist_ok=True)
            os.symlink(real_gh_config, test_gh_config)
        try:
            token_result = subprocess.run(
                ["gh", "auth", "token"], capture_output=True, text=True, check=False,
            )
            if token_result.returncode == 0 and token_result.stdout.strip():
                self.env["GH_TOKEN"] = token_result.stdout.strip()
        except FileNotFoundError:
            pass

        # Set up global git credential helper
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

        # SSH dir
        (self.home / ".ssh").mkdir()
        (self.home / ".ssh" / "config").write_text("")

        # Find workmux binary
        workmux_dir = Path(__file__).parent.parent
        self.binary = workmux_dir / "target" / "debug" / "workmux"
        if not self.binary.exists():
            pytest.fail("workmux binary not found. Run 'cargo build' first.")

    def create_repo(self, name: str) -> Path:
        """Create a local git repo."""
        repo = self.tmp_path / name
        repo.mkdir()
        self._git(repo, "init", "-b", "main")
        self._git(repo, "config", "user.email", "test@test.com")
        self._git(repo, "config", "user.name", "test")
        (repo / "README.md").write_text(f"# {name}\n")
        self._git(repo, "add", ".")
        self._git(repo, "commit", "-m", "Initial commit")
        return repo

    def _git(self, cwd: Path, *args):
        result = subprocess.run(
            ["git"] + list(args),
            cwd=cwd, capture_output=True, text=True, check=False, env=self.env,
        )
        if result.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr}")
        return result

    def write_config(self, config: dict):
        config.setdefault("agent", "echo")
        self.global_config_path.parent.mkdir(parents=True, exist_ok=True)
        self.global_config_path.write_text(yaml.dump(config))

    def run_workmux(self, *args, check: bool = False, cwd: Path | None = None) -> subprocess.CompletedProcess:
        result = subprocess.run(
            [str(self.binary)] + list(args),
            env=self.env, capture_output=True, text=True, check=False,
            cwd=cwd, timeout=120,
        )
        if check and result.returncode != 0:
            raise subprocess.CalledProcessError(
                result.returncode, result.args, result.stdout, result.stderr
            )
        return result

    def run_group_add(self, group_name: str, branch: str) -> subprocess.CompletedProcess:
        """Run group add and verify the workspace was created.
        
        group add may return non-zero if the agent launch fails (e.g., cmux
        surface mismatch in test environments) even when the workspace and
        dev_env were created successfully. This helper asserts on workspace
        existence rather than exit code.
        """
        result = self.run_workmux("group", "add", group_name, branch, "--background")
        ws_dir = self.group_workspace_dir(group_name, branch)
        assert ws_dir.exists(), (
            f"group add failed to create workspace.\n"
            f"exit={result.returncode}\nstdout={result.stdout}\nstderr={result.stderr}"
        )
        return result

    def load_group_state(self, group_name: str, branch: str) -> dict:
        slug = branch.replace("/", "-")
        ws_dir = self.groups_dir / f"{group_name}--{slug}"
        state_file = ws_dir / ".workmux-group.yaml"
        return yaml.safe_load(state_file.read_text())

    def group_workspace_dir(self, group_name: str, branch: str) -> Path:
        slug = branch.replace("/", "-")
        return self.groups_dir / f"{group_name}--{slug}"

    def ports_file(self) -> Path:
        return self.home / ".local" / "share" / "workmux" / "ports.yaml"


@pytest.fixture
def env(tmp_path):
    return DevEnvTestEnv(tmp_path)


# =============================================================================
# Config without codespace (no compute burned)
# =============================================================================


class TestDevEnvConfigLocal:
    """Test dev_env config parsing -- no codespaces created."""

    def test_group_add_without_dev_env_has_no_state(self, env: DevEnvTestEnv):
        """Groups without dev_env config have no dev_env state."""
        repo = env.create_repo("simple")
        env.write_config({
            "groups": {"test": {"repos": [{"path": str(repo)}]}},
        })
        env.run_group_add("test", "feat/no-dev")
        state = env.load_group_state("test", "feat/no-dev")
        assert state.get("dev_env") is None

    def test_detach_without_attach_errors(self, env: DevEnvTestEnv):
        """dev-env detach errors when nothing is attached."""
        repo = env.create_repo("bare")
        env.write_config({
            "groups": {"test": {"repos": [{"path": str(repo)}]}},
        })
        env.run_group_add("test", "feat/nodev")
        ws_dir = env.group_workspace_dir("test", "feat/nodev")
        result = env.run_workmux("group", "dev-env", "detach", cwd=ws_dir)
        assert result.returncode != 0

    def test_status_without_attach(self, env: DevEnvTestEnv):
        """dev-env status shows 'not attached' when nothing is attached."""
        repo = env.create_repo("bare")
        env.write_config({
            "groups": {"test": {"repos": [{"path": str(repo)}]}},
        })
        env.run_group_add("test", "feat/nodev2")
        ws_dir = env.group_workspace_dir("test", "feat/nodev2")
        result = env.run_workmux("group", "dev-env", "status", cwd=ws_dir)
        assert result.returncode == 0
        assert "no dev environment" in result.stdout.lower()

    def test_ports_without_attach(self, env: DevEnvTestEnv):
        """dev-env ports --json shows empty when nothing is attached."""
        repo = env.create_repo("bare")
        env.write_config({
            "groups": {"test": {"repos": [{"path": str(repo)}]}},
        })
        env.run_group_add("test", "feat/nodev3")
        ws_dir = env.group_workspace_dir("test", "feat/nodev3")
        result = env.run_workmux("group", "dev-env", "ports", "--json", cwd=ws_dir)
        assert result.returncode == 0
        assert json.loads(result.stdout) == []


# =============================================================================
# Full lifecycle with real codespace
# =============================================================================


@requires_codespace
class TestDevEnvLifecycle:
    """Full dev-env lifecycle against a real codespace.

    Creates ONE codespace and exercises the full flow:
    attach -> status -> ports -> detach -> re-attach -> remove
    """

    def test_full_lifecycle(self, env: DevEnvTestEnv):
        """End-to-end: auto-attach via group add, status, ports, detach, re-attach, group remove."""
        repo = env.create_repo("app")
        codespace_name = None

        try:
            # -- Step 1: group add with dev_env config (auto-attach) --
            env.write_config({
                "groups": {
                    "test": {
                        "repos": [{"path": str(repo)}],
                        "dev_env": {
                            "type": "codespace",
                            "repo": TEST_REPO,
                            "machine": TEST_MACHINE,
                            "idle_timeout": "5m",
                            "retention_period": "1h",
                            "ports": [8080],
                            "sync": "git-push",
                            "devloop": "echo build && echo test",
                        },
                    }
                }
            })

            env.run_group_add("test", "feat/lifecycle")

            state = env.load_group_state("test", "feat/lifecycle")
            dev = state.get("dev_env")
            assert dev is not None, "dev_env not in group state after add"
            codespace_name = dev.get("codespace_name")
            assert codespace_name is not None, "no codespace_name in state"
            assert dev.get("ssh_host") is not None, "no ssh_host in state"
            assert dev.get("remote_workdir") is not None, "no remote_workdir in state"
            assert dev.get("attached_at") is not None, "no attached_at in state"

            # Verify port mapping
            mappings = dev.get("port_mappings", [])
            assert len(mappings) == 1, f"expected 1 port mapping, got {len(mappings)}"
            assert mappings[0]["remote"] == 8080
            assert isinstance(mappings[0]["local"], int)
            assert 10000 <= mappings[0]["local"] <= 19999

            # Verify ports.yaml persisted
            assert env.ports_file().exists()
            pool = yaml.safe_load(env.ports_file().read_text())
            assert len(pool["allocations"]) == 1

            ws_dir = env.group_workspace_dir("test", "feat/lifecycle")

            # -- Step 2: SSH config written --
            ssh_config = (env.home / ".ssh" / "config").read_text()
            ssh_host = dev["ssh_host"]
            assert f"Host {ssh_host}" in ssh_config

            # -- Step 3: dev-env status --
            result = env.run_workmux("group", "dev-env", "status", cwd=ws_dir)
            assert result.returncode == 0, f"status failed: {result.stderr}"
            assert TEST_REPO in result.stdout
            assert codespace_name in result.stdout

            # -- Step 4: dev-env ports --
            result = env.run_workmux("group", "dev-env", "ports", "--json", cwd=ws_dir)
            assert result.returncode == 0, f"ports failed: {result.stderr}"
            port_data = json.loads(result.stdout)
            assert len(port_data) == 1
            assert port_data[0]["remote"] == 8080

            # -- Step 5: dev-env detach --
            result = env.run_workmux("group", "dev-env", "detach", cwd=ws_dir)
            assert result.returncode == 0, f"detach failed: {result.stderr}"

            # Verify state cleared
            state = env.load_group_state("test", "feat/lifecycle")
            assert state.get("dev_env") is None, "dev_env should be None after detach"

            # Verify ports released
            pool = yaml.safe_load(env.ports_file().read_text())
            assert len(pool["allocations"]) == 0, "ports not released after detach"

            # -- Step 6: manual re-attach --
            result = env.run_workmux(
                "group", "dev-env", "attach",
                "--codespace", codespace_name,
                "--repo", TEST_REPO,
                "--ports", "9090",
                cwd=ws_dir,
            )
            assert result.returncode == 0, f"re-attach failed: {result.stderr}\n{result.stdout}"

            state = env.load_group_state("test", "feat/lifecycle")
            dev = state.get("dev_env")
            assert dev is not None, "dev_env not in state after re-attach"
            assert dev["codespace_name"] == codespace_name
            assert len(dev["port_mappings"]) == 1
            assert dev["port_mappings"][0]["remote"] == 9090

            # -- Step 7: double attach errors --
            result = env.run_workmux(
                "group", "dev-env", "attach", "--repo", "org/other", cwd=ws_dir,
            )
            assert result.returncode != 0
            assert "already attached" in result.stderr.lower()

            # -- Step 8: group remove cleans up everything --
            result = env.run_workmux("group", "remove", "test", "feat/lifecycle", "-f")
            assert result.returncode == 0, f"remove failed: {result.stderr}"

            # Workspace gone
            assert not ws_dir.exists()

            # Ports released
            pool = yaml.safe_load(env.ports_file().read_text())
            assert len(pool["allocations"]) == 0

        finally:
            # Always clean up the codespace
            if codespace_name:
                cleanup_codespace(codespace_name)

    def test_port_isolation_across_groups(self, env: DevEnvTestEnv):
        """Two groups with dev_env get non-overlapping ports."""
        repo1 = env.create_repo("app1")
        repo2 = env.create_repo("app2")
        cs_name_1 = None
        cs_name_2 = None

        try:
            env.write_config({
                "groups": {
                    "g1": {
                        "repos": [{"path": str(repo1)}],
                        "dev_env": {
                            "type": "codespace",
                            "repo": TEST_REPO,
                            "machine": TEST_MACHINE,
                            "idle_timeout": "5m",
                            "retention_period": "1h",
                            "ports": [3000, 3001],
                        },
                    },
                    "g2": {
                        "repos": [{"path": str(repo2)}],
                        "dev_env": {
                            "type": "codespace",
                            "repo": TEST_REPO,
                            "machine": TEST_MACHINE,
                            "idle_timeout": "5m",
                            "retention_period": "1h",
                            "ports": [4000],
                        },
                    },
                }
            })

            env.run_group_add("g1", "feat/iso-a")
            s1 = env.load_group_state("g1", "feat/iso-a")
            cs_name_1 = s1["dev_env"]["codespace_name"]

            env.run_group_add("g2", "feat/iso-b")
            s2 = env.load_group_state("g2", "feat/iso-b")
            cs_name_2 = s2["dev_env"]["codespace_name"]

            locals1 = {m["local"] for m in s1["dev_env"]["port_mappings"]}
            locals2 = {m["local"] for m in s2["dev_env"]["port_mappings"]}

            assert len(locals1) == 2
            assert len(locals2) == 1
            assert locals1.isdisjoint(locals2), f"Port collision: {locals1} & {locals2}"

        finally:
            # Clean up both groups and codespaces
            env.run_workmux("group", "remove", "g1", "feat/iso-a", "-f")
            env.run_workmux("group", "remove", "g2", "feat/iso-b", "-f")
            if cs_name_1:
                cleanup_codespace(cs_name_1)
            if cs_name_2:
                cleanup_codespace(cs_name_2)

    def test_auto_attach_without_machine_picks_default(self, env: DevEnvTestEnv):
        """dev_env config without machine auto-selects the cheapest available."""
        repo = env.create_repo("app")
        codespace_name = None

        try:
            env.write_config({
                "groups": {
                    "test": {
                        "repos": [{"path": str(repo)}],
                        "dev_env": {
                            "type": "codespace",
                            "repo": TEST_REPO,
                            # no machine specified -- should auto-resolve
                            "idle_timeout": "5m",
                            "retention_period": "1h",
                        },
                    }
                }
            })

            env.run_group_add("test", "feat/no-machine")

            state = env.load_group_state("test", "feat/no-machine")
            dev = state.get("dev_env")
            assert dev is not None, "dev_env not in state"
            codespace_name = dev.get("codespace_name")
            assert codespace_name is not None, "codespace not created"
            assert dev.get("ssh_host") is not None, "ssh not configured"

        finally:
            env.run_workmux("group", "remove", "test", "feat/no-machine", "-f")
            if codespace_name:
                cleanup_codespace(codespace_name)


# =============================================================================
# Watcher mechanics (uses real codespace)
# =============================================================================


@requires_codespace
class TestWatcher:
    """Test watcher process mechanics with real codespace."""

    def test_watcher_pid_and_log(self, env: DevEnvTestEnv):
        """Watcher writes PID file and log with tunnel events."""
        repo = env.create_repo("app")
        codespace_name = None

        try:
            env.write_config({
                "groups": {
                    "test": {
                        "repos": [{"path": str(repo)}],
                        "dev_env": {
                            "type": "codespace",
                            "repo": TEST_REPO,
                            "machine": TEST_MACHINE,
                            "idle_timeout": "5m",
                            "retention_period": "1h",
                            "ports": [7000],
                        },
                    }
                }
            })

            env.run_group_add("test", "feat/watcher")

            state = env.load_group_state("test", "feat/watcher")
            codespace_name = state["dev_env"]["codespace_name"]
            watcher_pid = state["dev_env"].get("watcher_pid")

            assert watcher_pid is not None, "watcher_pid should be set when ports are configured"

            watcher_dir = env.home / ".local" / "share" / "workmux" / "watcher" / "test--feat-watcher"

            # PID file should appear
            pid_file = watcher_dir / "watcher.pid"
            for _ in range(20):
                if pid_file.exists():
                    break
                time.sleep(0.2)
            assert pid_file.exists(), f"PID file not found at {pid_file}"
            assert pid_file.read_text().strip().isdigit()

            # Log file should have at least one event
            log_file = watcher_dir / "watcher.log"
            for _ in range(30):
                if log_file.exists() and log_file.stat().st_size > 0:
                    break
                time.sleep(0.5)
            assert log_file.exists(), "watcher log not created"

            lines = log_file.read_text().strip().split("\n")
            assert len(lines) >= 1, "watcher log is empty"
            first_event = json.loads(lines[0])
            assert "ts" in first_event
            assert "tunnel" in first_event
            assert "event" in first_event
            assert "7000" in first_event["tunnel"]

            # Watcher process should be alive
            try:
                os.kill(watcher_pid, 0)
            except OSError:
                pytest.fail(f"Watcher PID {watcher_pid} is not running")

            # Detach should kill it
            ws_dir = env.group_workspace_dir("test", "feat/watcher")
            env.run_workmux("group", "dev-env", "detach", cwd=ws_dir, check=True)

            time.sleep(1)
            try:
                os.kill(watcher_pid, 0)
                pytest.fail(f"Watcher PID {watcher_pid} still alive after detach")
            except OSError:
                pass  # dead, as expected

        finally:
            env.run_workmux("group", "remove", "test", "feat/watcher", "-f")
            if codespace_name:
                cleanup_codespace(codespace_name)


# =============================================================================
# Cleanup check
# =============================================================================


@requires_codespace
class TestCleanup:
    """Verify no codespaces leaked from test runs."""

    def test_no_leaked_codespaces(self):
        """Check that no test codespaces are left running."""
        codespaces = list_test_codespaces()
        running = [cs for cs in codespaces if cs["state"] in ("Available", "Starting")]
        if running:
            # Clean them up
            for cs in running:
                cleanup_codespace(cs["name"])
            pytest.fail(
                f"Found {len(running)} leaked codespace(s): "
                f"{[cs['name'] for cs in running]}"
            )


@requires_codespace
class TestMultipleCodespaces:
    """Test codespace selection when multiple exist for a repo."""

    def test_reuses_running_codespace_over_shutdown(self, env: DevEnvTestEnv):
        """When multiple codespaces exist, prefer the running one.

        Creates two codespaces for the same repo, forces one into a non-Available
        state, and verifies workmux auto-attach picks the Available one.
        """
        repo = env.create_repo("app")
        cs_shutdown = None
        cs_running = None

        def wait_for_state(name: str, allowed: set[str], timeout_s: int = 120) -> str | None:
            """Poll state via `gh codespace list` to avoid side effects/races."""
            deadline = time.time() + timeout_s
            last = None
            while time.time() < deadline:
                check = subprocess.run(
                    ["gh", "codespace", "list", "--repo", TEST_REPO, "--json", "name,state"],
                    capture_output=True, text=True, check=False, timeout=20,
                )
                if check.returncode == 0:
                    try:
                        rows = json.loads(check.stdout)
                        row = next((r for r in rows if r.get("name") == name), None)
                        if row:
                            last = row.get("state")
                            if last in allowed:
                                return last
                    except json.JSONDecodeError:
                        pass
                time.sleep(2)
            return last

        try:
            # Create codespace that we'll stop.
            cs_shutdown = subprocess.run(
                [
                    "gh", "codespace", "create",
                    "--repo", TEST_REPO,
                    "--machine", TEST_MACHINE,
                    "--idle-timeout", "5m",
                    "--retention-period", "1h",
                ],
                capture_output=True, text=True, check=True, timeout=180,
            ).stdout.strip()

            # Ensure it actually reached Available before we stop it.
            ready_state = wait_for_state(cs_shutdown, {"Available"}, timeout_s=180)
            if ready_state != "Available":
                pytest.skip(f"First codespace never became Available (state: {ready_state})")

            stop = subprocess.run(
                ["gh", "codespace", "stop", "--codespace", cs_shutdown],
                capture_output=True, text=True, check=False, timeout=30,
            )
            # `gh codespace stop` returns non-zero when already not running.
            if stop.returncode != 0 and "not running" not in stop.stderr.lower():
                pytest.skip(f"failed to stop setup codespace: {stop.stderr}")

            # Wait until it is no longer Available.
            shutdown_state = wait_for_state(
                cs_shutdown,
                {"Shutdown", "ShuttingDown", "Stopped"},
                timeout_s=240,
            )
            if shutdown_state not in {"Shutdown", "ShuttingDown", "Stopped"}:
                pytest.skip(
                    "Could not force first codespace into non-Available state "
                    f"(last state: {shutdown_state})"
                )

            # Create second codespace that should remain Available.
            cs_running = subprocess.run(
                [
                    "gh", "codespace", "create",
                    "--repo", TEST_REPO,
                    "--machine", TEST_MACHINE,
                    "--idle-timeout", "5m",
                    "--retention-period", "1h",
                ],
                capture_output=True, text=True, check=True, timeout=180,
            ).stdout.strip()

            running_state = wait_for_state(cs_running, {"Available"}, timeout_s=180)
            if running_state != "Available":
                pytest.skip(f"Second codespace did not reach Available (state: {running_state})")

            env.write_config({
                "groups": {
                    "test": {
                        "repos": [{"path": str(repo)}],
                        "dev_env": {
                            "type": "codespace",
                            "repo": TEST_REPO,
                            "machine": TEST_MACHINE,
                            "idle_timeout": "5m",
                            "retention_period": "1h",
                        },
                    }
                }
            })

            env.run_group_add("test", "feat/multi-cs")

            state = env.load_group_state("test", "feat/multi-cs")
            attached_cs = state["dev_env"]["codespace_name"]

            assert attached_cs == cs_running, (
                "Expected workmux to pick the Available codespace when multiple exist. "
                f"shutdown={cs_shutdown}, running={cs_running}, attached={attached_cs}"
            )

        finally:
            env.run_workmux("group", "remove", "test", "feat/multi-cs", "-f")
            if cs_shutdown:
                cleanup_codespace(cs_shutdown)
            if cs_running:
                cleanup_codespace(cs_running)
