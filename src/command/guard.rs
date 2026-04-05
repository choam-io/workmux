//! `workmux guard` - install pre-commit hooks on ghq main branches to prevent
//! accidental direct commits. Encourages worktree-based workflows.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const HOOK_SCRIPT: &str = r#"#!/bin/sh
# Installed by `workmux guard` -- prevents direct commits to the main checkout.
# Work in worktrees instead: `workmux add <branch>`
# To bypass (e.g. dotfile changes): git commit --no-verify
echo "error: direct commits to main checkout blocked. Use workmux add <branch>." >&2
exit 1
"#;

const HOOK_MARKER: &str = "workmux guard";

/// Install the guard hook in a single repository.
fn install_hook(repo_path: &Path) -> Result<bool> {
    let dot_git = repo_path.join(".git");

    // Worktrees have a .git *file* (not directory) -- skip them
    if dot_git.is_file() {
        return Ok(false);
    }

    // Must have a .git directory to be a real repo checkout
    if !dot_git.is_dir() {
        return Ok(false);
    }

    let hooks_dir = dot_git.join("hooks");
    if !hooks_dir.exists() {
        fs::create_dir_all(&hooks_dir)
            .with_context(|| format!("Failed to create hooks dir: {}", hooks_dir.display()))?;
    }

    let hook_path = hooks_dir.join("pre-commit");

    // Don't overwrite an existing hook that isn't ours
    if hook_path.exists() {
        let content = fs::read_to_string(&hook_path).unwrap_or_default();
        if !content.contains(HOOK_MARKER) {
            eprintln!(
                "  skip: {} (existing pre-commit hook, not ours)",
                repo_path.display()
            );
            return Ok(false);
        }
        // Our hook already installed
        return Ok(false);
    }

    fs::write(&hook_path, HOOK_SCRIPT)
        .with_context(|| format!("Failed to write hook: {}", hook_path.display()))?;

    // Make executable
    let mut perms = fs::metadata(&hook_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook_path, perms)?;

    Ok(true)
}

/// Remove the guard hook from a single repository.
fn remove_hook(repo_path: &Path) -> Result<bool> {
    let hook_path = repo_path.join(".git").join("hooks").join("pre-commit");

    if !hook_path.exists() {
        return Ok(false);
    }

    let content = fs::read_to_string(&hook_path).unwrap_or_default();
    if !content.contains(HOOK_MARKER) {
        // Not our hook, don't touch it
        return Ok(false);
    }

    fs::remove_file(&hook_path)
        .with_context(|| format!("Failed to remove hook: {}", hook_path.display()))?;

    Ok(true)
}

/// Find all ghq-managed repositories.
fn find_ghq_repos() -> Result<Vec<PathBuf>> {
    let home = home::home_dir().context("Could not determine home directory")?;
    let ghq_root = home.join("ghq");

    if !ghq_root.exists() {
        return Ok(Vec::new());
    }

    let mut repos = Vec::new();
    find_git_repos(&ghq_root, &mut repos, 0)?;
    repos.sort();
    Ok(repos)
}

/// Recursively find directories containing .git (max depth 5).
fn find_git_repos(dir: &Path, repos: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    if depth > 5 {
        return Ok(());
    }

    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        // Check for bare repo (has no worktree, just git internals)
        let head = dot_git.join("HEAD");
        let is_bare = !head.exists();
        if is_bare {
            return Ok(()); // Skip bare repos
        }
        repos.push(dir.to_path_buf());
        return Ok(()); // Don't recurse into git repos
    }

    // Also skip directories that ARE bare repos (HEAD at top level, no .git subdir)
    if dir.join("HEAD").exists() && dir.join("objects").exists() && dir.join("refs").exists() {
        return Ok(());
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip __worktrees directories
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.contains("__worktrees") {
                    continue;
                }
                find_git_repos(&path, repos, depth + 1)?;
            }
        }
    }

    Ok(())
}

/// Run `workmux guard` to install hooks across all ghq repos.
pub fn run_install() -> Result<()> {
    let repos = find_ghq_repos()?;

    if repos.is_empty() {
        println!("No ghq repositories found in ~/ghq");
        return Ok(());
    }

    let mut installed = 0;
    let mut skipped = 0;

    for repo in &repos {
        match install_hook(repo) {
            Ok(true) => {
                println!("  ✓ {}", repo.display());
                installed += 1;
            }
            Ok(false) => {
                skipped += 1;
            }
            Err(e) => {
                eprintln!("  ✗ {}: {}", repo.display(), e);
            }
        }
    }

    println!(
        "\nGuard hooks: {} installed, {} skipped (already guarded or have custom hooks)",
        installed, skipped
    );
    Ok(())
}

/// Run `workmux guard --remove` to uninstall hooks.
pub fn run_remove() -> Result<()> {
    let repos = find_ghq_repos()?;

    let mut removed = 0;

    for repo in &repos {
        match remove_hook(repo) {
            Ok(true) => {
                println!("  ✓ removed: {}", repo.display());
                removed += 1;
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("  ✗ {}: {}", repo.display(), e);
            }
        }
    }

    println!("\nGuard hooks removed: {}", removed);
    Ok(())
}

/// Run `workmux guard --status` to show current state.
pub fn run_status() -> Result<()> {
    let repos = find_ghq_repos()?;

    let mut guarded = 0;
    let mut unguarded = 0;
    let mut custom = 0;

    for repo in &repos {
        let hook_path = repo.join(".git").join("hooks").join("pre-commit");
        if hook_path.exists() {
            let content = fs::read_to_string(&hook_path).unwrap_or_default();
            if content.contains(HOOK_MARKER) {
                guarded += 1;
            } else {
                custom += 1;
            }
        } else {
            unguarded += 1;
        }
    }

    println!("ghq repos: {}", repos.len());
    println!("  guarded:   {}", guarded);
    println!("  unguarded: {}", unguarded);
    if custom > 0 {
        println!("  custom:    {} (have non-workmux pre-commit hooks)", custom);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn make_git_repo(dir: &Path) {
        let git_dir = dir.join(".git");
        fs::create_dir_all(git_dir.join("hooks")).unwrap();
        // Minimal .git structure so it looks like a repo
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn test_install_hook_creates_executable_hook() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(tmp.path());

        let installed = install_hook(tmp.path()).unwrap();
        assert!(installed);

        let hook_path = tmp.path().join(".git/hooks/pre-commit");
        assert!(hook_path.exists());

        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains(HOOK_MARKER));
        assert!(content.contains("exit 1"));

        // Check executable
        let perms = fs::metadata(&hook_path).unwrap().permissions();
        assert!(perms.mode() & 0o111 != 0);
    }

    #[test]
    fn test_install_hook_idempotent() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(tmp.path());

        assert!(install_hook(tmp.path()).unwrap()); // first install
        assert!(!install_hook(tmp.path()).unwrap()); // second is no-op
    }

    #[test]
    fn test_install_hook_skips_existing_custom_hook() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(tmp.path());

        let hook_path = tmp.path().join(".git/hooks/pre-commit");
        fs::write(&hook_path, "#!/bin/sh\necho custom lint\n").unwrap();

        let installed = install_hook(tmp.path()).unwrap();
        assert!(!installed);

        // Verify original hook is untouched
        let content = fs::read_to_string(&hook_path).unwrap();
        assert!(content.contains("custom lint"));
        assert!(!content.contains(HOOK_MARKER));
    }

    #[test]
    fn test_remove_hook_removes_our_hook() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(tmp.path());

        install_hook(tmp.path()).unwrap();
        let hook_path = tmp.path().join(".git/hooks/pre-commit");
        assert!(hook_path.exists());

        let removed = remove_hook(tmp.path()).unwrap();
        assert!(removed);
        assert!(!hook_path.exists());
    }

    #[test]
    fn test_remove_hook_leaves_custom_hook_alone() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(tmp.path());

        let hook_path = tmp.path().join(".git/hooks/pre-commit");
        fs::write(&hook_path, "#!/bin/sh\necho my hook\n").unwrap();

        let removed = remove_hook(tmp.path()).unwrap();
        assert!(!removed);
        assert!(hook_path.exists());
    }

    #[test]
    fn test_remove_hook_noop_when_no_hook() {
        let tmp = TempDir::new().unwrap();
        make_git_repo(tmp.path());

        let removed = remove_hook(tmp.path()).unwrap();
        assert!(!removed);
    }

    #[test]
    fn test_install_hook_skips_worktree_git_file() {
        let tmp = TempDir::new().unwrap();
        // Worktrees have a .git *file*, not directory
        fs::write(
            tmp.path().join(".git"),
            "gitdir: /somewhere/else/.git/worktrees/foo\n",
        )
        .unwrap();

        let installed = install_hook(tmp.path()).unwrap();
        assert!(!installed);
    }

    #[test]
    fn test_install_creates_hooks_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        // No hooks/ subdirectory yet

        let installed = install_hook(tmp.path()).unwrap();
        assert!(installed);
        assert!(tmp.path().join(".git/hooks/pre-commit").exists());
    }

    #[test]
    fn test_find_git_repos_skips_bare_repos() {
        let tmp = TempDir::new().unwrap();
        let ghq = tmp.path().join("ghq");

        // Normal repo
        let normal = ghq.join("github.com/org/normal");
        fs::create_dir_all(normal.join(".git/hooks")).unwrap();
        fs::write(normal.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        // Bare repo (has HEAD, objects, refs at top level, no .git subdir)
        let bare = ghq.join("github.com/org/bare.git");
        fs::create_dir_all(bare.join("objects")).unwrap();
        fs::create_dir_all(bare.join("refs")).unwrap();
        fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let mut repos = Vec::new();
        find_git_repos(&ghq, &mut repos, 0).unwrap();

        let names: Vec<&str> = repos
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        assert!(names.contains(&"normal"), "should find normal repo");
        assert!(!names.contains(&"bare.git"), "should skip bare repo");
    }
}
