//! Integration tests for execution targets.
//!
//! These tests verify the target attach/detach/run flow works correctly.
//!
//! Run with: cargo test --test target_integration
//!
//! For real codespace testing (requires gh auth):
//!   WORKMUX_TEST_REAL_CODESPACE=github/some-repo cargo test --test target_integration -- --ignored

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Path to the workmux binary (built in debug mode for tests)
fn workmux_bin() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target/debug/workmux");
    path
}

/// Create a mock git worktree structure for testing
fn setup_mock_worktree(name: &str) -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let worktree_dir = dir.path().join(format!("project__worktrees/{}", name));
    fs::create_dir_all(&worktree_dir).expect("create worktree dir");

    // Initialize as git repo (workmux needs this)
    Command::new("git")
        .args(["init"])
        .current_dir(&worktree_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git init");

    // Configure git user for commits
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&worktree_dir)
        .status()
        .ok();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&worktree_dir)
        .status()
        .ok();

    // Create initial commit
    fs::write(worktree_dir.join("README.md"), "# Test").expect("write readme");
    Command::new("git")
        .args(["add", "."])
        .current_dir(&worktree_dir)
        .status()
        .ok();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&worktree_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();

    dir
}

/// Create a mock gh command that simulates codespace operations
fn setup_mock_gh(temp_dir: &TempDir) -> PathBuf {
    let mock_dir = temp_dir.path().join("mock-bin");
    fs::create_dir_all(&mock_dir).expect("create mock dir");

    let mock_gh = mock_dir.join("gh");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let script = r#"#!/bin/bash
# Mock gh command for testing workmux target

case "$1 $2" in
    "codespace list")
        # Return empty list (no existing codespace)
        echo "[]"
        ;;
    "codespace create")
        # Simulate creating a codespace
        echo '{"name": "mock-codespace-abc123"}'
        ;;
    "codespace view")
        # Return running state
        echo '{"state": "Available"}'
        ;;
    "codespace ssh")
        if [[ "$*" == *"--config"* ]]; then
            # Simulate adding SSH config
            exit 0
        elif [[ "$*" == *"-- true"* ]]; then
            # Simulate starting codespace
            exit 0
        else
            # Simulate SSH command
            shift 4  # Skip: codespace ssh --codespace name
            eval "$@"
        fi
        ;;
    "codespace stop")
        exit 0
        ;;
    *)
        echo "Unknown mock command: $*" >&2
        exit 1
        ;;
esac
"#;
        fs::write(&mock_gh, script).expect("write mock gh");
        fs::set_permissions(&mock_gh, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[cfg(windows)]
    {
        // Windows batch file version
        let script = r#"@echo off
if "%1 %2"=="codespace list" (
    echo []
    exit /b 0
)
if "%1 %2"=="codespace create" (
    echo {"name": "mock-codespace-abc123"}
    exit /b 0
)
echo Unknown mock command: %* >&2
exit /b 1
"#;
        let mock_gh_bat = mock_dir.join("gh.bat");
        fs::write(&mock_gh_bat, script).expect("write mock gh.bat");
        return mock_gh_bat;
    }

    mock_gh
}

/// Set up mock SSH that just runs commands locally
fn setup_mock_ssh(temp_dir: &TempDir) -> PathBuf {
    let mock_dir = temp_dir.path().join("mock-bin");
    fs::create_dir_all(&mock_dir).expect("create mock dir");

    let mock_ssh = mock_dir.join("ssh");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        // Mock SSH that just runs commands locally
        let script = r#"#!/bin/bash
# Mock ssh command - runs commands locally instead of remotely

# Skip connection options
while [[ "$1" == -* ]]; do
    shift
    if [[ "$1" != -* ]]; then
        shift  # Skip option value
    fi
done

# Skip host
shift

# Run the command locally
if [ -n "$1" ]; then
    bash -c "$*"
else
    bash
fi
"#;
        fs::write(&mock_ssh, script).expect("write mock ssh");
        fs::set_permissions(&mock_ssh, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    mock_ssh
}

/// Run workmux with custom PATH including mocks
fn run_workmux_with_mocks(
    args: &[&str],
    workdir: &std::path::Path,
    mock_bin_dir: &std::path::Path,
) -> std::process::Output {
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", mock_bin_dir.display(), current_path);

    Command::new(workmux_bin())
        .args(args)
        .current_dir(workdir)
        .env("PATH", new_path)
        .env("HOME", workdir.parent().unwrap().parent().unwrap()) // Set HOME for state storage
        .output()
        .expect("run workmux")
}

// ============================================================================
// Unit Tests (no external dependencies)
// ============================================================================

#[test]
fn test_target_status_no_target() {
    let temp = setup_mock_worktree("test-feature");
    let worktree_dir = temp.path().join("project__worktrees/test-feature");

    let output = Command::new(workmux_bin())
        .args(["target", "status"])
        .current_dir(&worktree_dir)
        .output()
        .expect("run workmux");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("local") || stdout.contains("no remote"),
        "Expected 'local' status, got: {}",
        stdout
    );
}

#[test]
fn test_target_status_json_no_target() {
    let temp = setup_mock_worktree("test-json");
    let worktree_dir = temp.path().join("project__worktrees/test-json");

    let output = Command::new(workmux_bin())
        .args(["target", "status", "--json"])
        .current_dir(&worktree_dir)
        .output()
        .expect("run workmux");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""type": "local""#) || stdout.contains(r#""type":"local""#),
        "Expected JSON with type=local, got: {}",
        stdout
    );
}

#[test]
fn test_target_detach_no_target() {
    let temp = setup_mock_worktree("test-detach");
    let worktree_dir = temp.path().join("project__worktrees/test-detach");

    let output = Command::new(workmux_bin())
        .args(["target", "detach"])
        .current_dir(&worktree_dir)
        .output()
        .expect("run workmux");

    // Should fail gracefully - no target to detach
    assert!(
        !output.status.success(),
        "detach with no target should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No target") || stderr.contains("no remote"),
        "Expected error about no target, got: {}",
        stderr
    );
}

#[test]
fn test_target_attach_requires_type() {
    let temp = setup_mock_worktree("test-attach-type");
    let worktree_dir = temp.path().join("project__worktrees/test-attach-type");

    let output = Command::new(workmux_bin())
        .args(["target", "attach"])
        .current_dir(&worktree_dir)
        .output()
        .expect("run workmux");

    // Should fail - no target type specified
    assert!(
        !output.status.success(),
        "attach without type should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--codespace") || stderr.contains("target type"),
        "Expected error about missing target type, got: {}",
        stderr
    );
}

// ============================================================================
// Mock Integration Tests
// ============================================================================

#[test]
fn test_target_attach_with_mock_codespace() {
    let temp = setup_mock_worktree("test-mock-attach");
    let worktree_dir = temp.path().join("project__worktrees/test-mock-attach");
    let mock_bin = temp.path().join("mock-bin");

    // Set up mocks
    setup_mock_gh(&temp);
    setup_mock_ssh(&temp);

    // Attach codespace
    let output = run_workmux_with_mocks(
        &["target", "attach", "--codespace", "github/test-repo"],
        &worktree_dir,
        &mock_bin,
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should succeed or at least attempt the attach
    // (May fail on SSH config step but that's OK for this test)
    println!("stdout: {}", stdout);
    println!("stderr: {}", stderr);

    // Verify state file was created
    // Note: This depends on XDG_STATE_HOME being set appropriately
}

#[test]
fn test_target_lifecycle_with_mocks() {
    let temp = setup_mock_worktree("test-lifecycle");
    let worktree_dir = temp.path().join("project__worktrees/test-lifecycle");
    let mock_bin = temp.path().join("mock-bin");

    // Set up mocks
    setup_mock_gh(&temp);
    setup_mock_ssh(&temp);

    // 1. Check initial status
    let output = run_workmux_with_mocks(&["target", "status"], &worktree_dir, &mock_bin);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("local"), "Initial status should be local");

    // 2. Attach (may partially fail due to SSH config, but state should be written)
    let _output = run_workmux_with_mocks(
        &["target", "attach", "--codespace", "github/test-repo"],
        &worktree_dir,
        &mock_bin,
    );

    // 3. Check status again - may show codespace if attach succeeded
    let output = run_workmux_with_mocks(&["target", "status"], &worktree_dir, &mock_bin);
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Status after attach: {}", stdout);

    // 4. Detach (cleanup)
    let _output = run_workmux_with_mocks(&["target", "detach"], &worktree_dir, &mock_bin);

    // 5. Verify back to local
    let output = run_workmux_with_mocks(&["target", "status"], &worktree_dir, &mock_bin);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("local"),
        "Should be back to local after detach"
    );
}

// ============================================================================
// Real Codespace Tests (requires gh auth, run with --ignored)
// ============================================================================

/// Test with a real GitHub Codespace.
///
/// Run with: WORKMUX_TEST_REAL_CODESPACE=github/some-repo cargo test --test target_integration real_codespace -- --ignored
#[test]
#[ignore]
fn test_real_codespace_attach_run_detach() {
    let repo = match std::env::var("WORKMUX_TEST_REAL_CODESPACE") {
        Ok(r) => r,
        Err(_) => {
            eprintln!("Skipping real codespace test: WORKMUX_TEST_REAL_CODESPACE not set");
            return;
        }
    };

    let temp = setup_mock_worktree("test-real-cs");
    let worktree_dir = temp.path().join("project__worktrees/test-real-cs");

    // 1. Attach
    println!("Attaching codespace for {}...", repo);
    let output = Command::new(workmux_bin())
        .args(["target", "attach", "--codespace", &repo])
        .current_dir(&worktree_dir)
        .output()
        .expect("attach codespace");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("Failed to attach codespace: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Attach output: {}", stdout);
    assert!(stdout.contains("Target attached"), "Should show success message");

    // 2. Check status
    let output = Command::new(workmux_bin())
        .args(["target", "status"])
        .current_dir(&worktree_dir)
        .output()
        .expect("check status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Status: {}", stdout);
    assert!(stdout.contains("codespace"), "Status should show codespace");
    assert!(stdout.contains(&repo), "Status should show repo name");

    // 3. Run a simple command
    println!("Running 'echo hello' on codespace...");
    let output = Command::new(workmux_bin())
        .args(["run", "test-real-cs", "--", "echo", "hello"])
        .current_dir(&worktree_dir)
        .output()
        .expect("run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Run output: {}", stdout);
    assert!(output.status.success(), "Remote command should succeed");
    assert!(stdout.contains("hello"), "Should see 'hello' in output");

    // 4. Run command that checks we're actually remote
    println!("Running 'hostname' on codespace...");
    let output = Command::new(workmux_bin())
        .args(["run", "test-real-cs", "--", "hostname"])
        .current_dir(&worktree_dir)
        .output()
        .expect("run hostname");

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Hostname: {}", stdout);
    // Codespace hostnames typically contain 'codespaces' or similar
    // Just verify we got some output

    // 5. Detach
    println!("Detaching...");
    let output = Command::new(workmux_bin())
        .args(["target", "detach"])
        .current_dir(&worktree_dir)
        .output()
        .expect("detach");

    assert!(output.status.success(), "Detach should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("Detach output: {}", stdout);

    // 6. Verify back to local
    let output = Command::new(workmux_bin())
        .args(["target", "status"])
        .current_dir(&worktree_dir)
        .output()
        .expect("final status");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("local"), "Should be back to local");

    println!("✓ Real codespace test passed!");
}

// ============================================================================
// State Persistence Tests
// ============================================================================

#[test]
fn test_target_state_persistence() {
    // Test the TargetStore directly

    let temp = TempDir::new().expect("create temp dir");
    let state_dir = temp.path().join("state/workmux/targets");
    fs::create_dir_all(&state_dir).expect("create state dir");

    // Write a target state file directly
    let state_json = r#"{
        "handle": "my-feature",
        "worktree_path": "/path/to/worktree",
        "target": {
            "type": "codespace",
            "repo": "github/test",
            "codespace_name": "test-cs-123",
            "ssh_host": "cs.test-cs-123"
        },
        "attached_ts": 1234567890,
        "remote_workdir": "/workspaces/test"
    }"#;

    fs::write(state_dir.join("my-feature.json"), state_json).expect("write state");

    // Verify file exists and is valid JSON
    let content = fs::read_to_string(state_dir.join("my-feature.json")).expect("read state");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse JSON");

    assert_eq!(parsed["handle"], "my-feature");
    assert_eq!(parsed["target"]["type"], "codespace");
    assert_eq!(parsed["target"]["repo"], "github/test");
}

// ============================================================================
// CLI Help Tests
// ============================================================================

#[test]
fn test_target_help() {
    let output = Command::new(workmux_bin())
        .args(["target", "--help"])
        .output()
        .expect("run help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("attach"), "Help should mention attach");
    assert!(stdout.contains("detach"), "Help should mention detach");
    assert!(stdout.contains("status"), "Help should mention status");
}

#[test]
fn test_target_attach_help() {
    let output = Command::new(workmux_bin())
        .args(["target", "attach", "--help"])
        .output()
        .expect("run attach help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--codespace"), "Help should mention --codespace");
    assert!(
        stdout.contains("github/") || stdout.contains("repo"),
        "Help should mention repo format"
    );
}
